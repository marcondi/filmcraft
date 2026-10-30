//! Text-based editing: the sequence transcript and the edits made through it.
//!
//! Clip transcripts (`filmcraft_project::Transcript`) hold words in **media time**. The sequence
//! transcript ([`sequence_words`]) maps them through the audio track items that play them:
//!
//! - audio tracks are read top first (A1, A2…); a word is taken from the first track whose
//!   transcribed clip covers the word's midpoint, so a dialogue clip duplicated on two tracks (or a
//!   stereo pair split over two mono tracks) reads once;
//! - a word belongs to a clip when its midpoint, mapped through the clip's speed, falls inside the
//!   clip; its timeline bounds are clamped to the clip, so a word cut by an edit is shown cut;
//! - disabled clips, reversed clips and frame holds contribute no words (they don't play speech).
//!
//! Text edits turn word ranges into timeline ranges ([`word_range`]) snapped outward to frames, then
//! reuse the ordinary [`crate::extract`] / [`crate::lift`] edits. Pause and filler-word removal
//! ([`find_pauses`], [`find_fillers`]) produce many ranges that [`ripple_delete_ranges`] removes in
//! one pass, right to left. Captions come from [`caption_blocks`].

use std::collections::BTreeMap;
use std::sync::Arc;

use filmcraft_project::{Caption, CaptionAlign, CaptionAnchor, CaptionStyle, ClipId, ItemId, Sequence, TrackId, Transcript};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick, TimeRange};

use crate::EditCtx;

/// A word of the sequence transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct SeqWord {
    pub text: String,
    /// Sequence time.
    pub start: Tick,
    pub end: Tick,
    /// The track item it is heard through and that item's media.
    pub clip: ClipId,
    pub item: ItemId,
    /// Index of the word in the media item's transcript.
    pub index: usize,
    /// Audio track index (0 = A1).
    pub track: usize,
    pub speaker: Option<String>,
    pub confidence: f32,
}

impl SeqWord {
    pub fn normalized(&self) -> String {
        filmcraft_project::transcript::normalize_word(&self.text)
    }
    pub fn range(&self) -> TimeRange {
        TimeRange::from_bounds(self.start, self.end.max(self.start))
    }
}

/// Transcripts by media item (as in `Project::transcripts`).
pub type Transcripts = BTreeMap<ItemId, Arc<Transcript>>;

fn ticks(t: f64) -> Tick {
    Tick(t.round() as i64)
}

/// The sequence transcript: words of every transcribed clip on the audio tracks, in sequence
/// time order (see the module docs for the rules).
pub fn sequence_words(seq: &Sequence, transcripts: &Transcripts) -> Vec<SeqWord> {
    let mut out: Vec<SeqWord> = Vec::new();
    // timeline ranges already served by a higher track's transcribed clips
    let mut claimed: Vec<TimeRange> = Vec::new();
    for (ti, track) in seq.audio_tracks.iter().enumerate() {
        let mut mine = Vec::new();
        for it in &track.items {
            if !it.enabled || it.reverse || it.frame_hold.is_some() || it.speed <= 0.0 {
                continue;
            }
            let Some(tr) = transcripts.get(&it.item) else { continue };
            mine.push(it.range());
            let speed = it.speed;
            let media_end = it.source_in + ticks(it.duration.0 as f64 * speed);
            let to_tl = |m: Tick| it.start + ticks((m - it.source_in).0 as f64 / speed);
            for wi in tr.words_in(TimeRange::from_bounds(it.source_in, media_end.max(it.source_in))) {
                let w = &tr.words[wi];
                let (a, b) = (to_tl(w.start), to_tl(w.end.max(w.start)));
                let mid = Tick(a.0 + (b.0 - a.0) / 2);
                if mid < it.start || mid >= it.end() || claimed.iter().any(|r| r.contains(mid)) {
                    continue;
                }
                out.push(SeqWord {
                    text: w.text.clone(),
                    start: a.max(it.start),
                    end: b.min(it.end()).max(a.max(it.start)),
                    clip: it.id,
                    item: it.item,
                    index: wi,
                    track: ti,
                    speaker: tr.speaker_name(w),
                    confidence: w.confidence,
                });
            }
        }
        claimed.extend(mine);
    }
    out.sort_by_key(|w| (w.start, w.track));
    out
}

/// Index of the word being spoken at `t` (the last word starting at or before `t` whose end is
/// after `t`).
pub fn word_at(words: &[SeqWord], t: Tick) -> Option<usize> {
    let i = words.partition_point(|w| w.start <= t).checked_sub(1)?;
    (t < words[i].end).then_some(i)
}

/// Paragraphs (Text panel segments): runs of words split where the speaker changes or at a pause
/// of at least `gap`.
pub fn paragraphs(words: &[SeqWord], gap: Tick) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut a = 0;
    for i in 1..=words.len() {
        if i == words.len() || words[i].speaker != words[i - 1].speaker || words[i].start - words[i - 1].end >= gap {
            if a < i {
                out.push(a..i);
            }
            a = i;
        }
    }
    out
}

/// Matches of `query` (one or more words, case and punctuation ignored; the last query word may be
/// a prefix) as word index ranges.
pub fn search(words: &[SeqWord], query: &str) -> Vec<std::ops::Range<usize>> {
    let q: Vec<String> = query.split_whitespace().map(filmcraft_project::transcript::normalize_word).filter(|s| !s.is_empty()).collect();
    if q.is_empty() {
        return Vec::new();
    }
    let norm: Vec<String> = words.iter().map(SeqWord::normalized).collect();
    let mut out = Vec::new();
    for i in 0..norm.len().saturating_sub(q.len() - 1) {
        let ok = q.iter().enumerate().all(|(k, qw)| if k + 1 == q.len() { norm[i + k].starts_with(qw.as_str()) } else { norm[i + k] == *qw });
        if ok {
            out.push(i..i + q.len());
        }
    }
    out
}

/// Timeline range of words `a..=b`, snapped outward to frames.
pub fn word_range(words: &[SeqWord], a: usize, b: usize, rate: FrameRate) -> Option<TimeRange> {
    let (a, b) = (a.min(b), a.max(b));
    let (s, e) = (words.get(a)?.start, words.get(b)?.end);
    let s = rate.snap(s);
    let mut e2 = rate.snap(e);
    if e2 < e || e2 <= s {
        e2 += rate.frame_duration();
    }
    Some(TimeRange::from_bounds(s, e2))
}

/// Media range of words `a..=b` of a clip transcript, snapped outward to the media's frames (for
/// Source-monitor In/Out from a text selection).
pub fn media_word_range(t: &Transcript, a: usize, b: usize, rate: FrameRate) -> Option<TimeRange> {
    let (a, b) = (a.min(b), a.max(b));
    let (s, e) = (t.words.get(a)?.start, t.words.get(b)?.end);
    let s = rate.snap(s);
    let mut e2 = rate.snap(e);
    if e2 < e || e2 <= s {
        e2 += rate.frame_duration();
    }
    Some(TimeRange::from_bounds(s, e2))
}

/// Pauses between consecutive words of at least `min`, as the ranges to remove: each keeps `keep`
/// of silence next to both words and is snapped inward to frames (pauses shorter than one frame
/// after that are skipped).
pub fn find_pauses(words: &[SeqWord], min: Tick, keep: Tick, rate: FrameRate) -> Vec<TimeRange> {
    let mut out = Vec::new();
    for p in words.windows(2) {
        let (a, b) = (p[0].end, p[1].start);
        if b - a < min || b <= a {
            continue;
        }
        let s = a + keep;
        let e = b - keep;
        let mut s2 = rate.snap(s);
        if s2 < s {
            s2 += rate.frame_duration();
        }
        let e2 = rate.snap(e);
        if e2 > s2 {
            out.push(TimeRange::from_bounds(s2, e2));
        }
    }
    out
}

/// The default filler words and phrases (configurable in preferences and per command).
pub const DEFAULT_FILLERS: &[&str] = &["um", "uh", "umm", "uhm", "erm", "er", "ah", "hmm", "mm", "mhm"];

/// Filler words: word index ranges matching one of `fillers` (each a word or a phrase such as
/// "you know"; case and punctuation ignored).
pub fn find_fillers(words: &[SeqWord], fillers: &[String]) -> Vec<std::ops::Range<usize>> {
    let norm: Vec<String> = words.iter().map(SeqWord::normalized).collect();
    let mut phrases: Vec<Vec<String>> = fillers
        .iter()
        .map(|f| f.split_whitespace().map(filmcraft_project::transcript::normalize_word).filter(|s| !s.is_empty()).collect::<Vec<_>>())
        .filter(|p| !p.is_empty())
        .collect();
    // longest phrases first, so "you know" wins over a lone "you"
    phrases.sort_by_key(|p| std::cmp::Reverse(p.len()));
    let mut out = Vec::new();
    let mut i = 0;
    while i < norm.len() {
        let hit = phrases.iter().find(|p| i + p.len() <= norm.len() && p.iter().enumerate().all(|(k, w)| norm[i + k] == *w));
        match hit {
            Some(p) => {
                out.push(i..i + p.len());
                i += p.len();
            }
            None => i += 1,
        }
    }
    out
}

/// Timeline ranges removing the filler words `hits`: each word range snapped to the nearest frames
/// (never into the neighbouring words' frames).
pub fn filler_ranges(words: &[SeqWord], hits: &[std::ops::Range<usize>], rate: FrameRate) -> Vec<TimeRange> {
    let mut out = Vec::new();
    for h in hits {
        let (a, b) = (h.start, h.end - 1);
        let mut s = rate.snap_nearest(words[a].start);
        let mut e = rate.snap_nearest(words[b].end);
        if a > 0 && s < words[a - 1].end {
            s = rate.snap(words[a - 1].end) + rate.frame_duration();
        }
        if let Some(n) = words.get(b + 1)
            && e > n.start
        {
            e = rate.snap(n.start);
        }
        if e > s {
            out.push(TimeRange::from_bounds(s, e));
        }
    }
    out
}

/// Sort and merge ranges (overlapping or touching).
pub fn merge_ranges(mut r: Vec<TimeRange>) -> Vec<TimeRange> {
    r.sort_by_key(|x| x.start);
    let mut out: Vec<TimeRange> = Vec::new();
    for x in r {
        match out.last_mut() {
            Some(l) if x.start <= l.end() => *l = TimeRange::from_bounds(l.start, l.end().max(x.end())),
            _ => out.push(x),
        }
    }
    out
}

/// Ripple-delete every range on every unlocked track (and sync-locked caption tracks), right to
/// left so earlier ranges keep their positions. Returns the total time removed.
pub fn ripple_delete_ranges(seq: &mut Sequence, ranges: Vec<TimeRange>, ctx: &mut EditCtx) -> Tick {
    let tracks: Vec<TrackId> = seq.all_tracks().filter(|t| !t.locked).map(|t| t.id).collect();
    let mut total = Tick::ZERO;
    for r in merge_ranges(ranges).into_iter().rev() {
        if r.duration <= Tick::ZERO {
            continue;
        }
        crate::extract(seq, &tracks, r, ctx);
        total += r.duration;
    }
    total
}

/// Rules for Create Captions from a transcript (Premiere's dialog defaults).
#[derive(Clone, Debug, PartialEq)]
pub struct CaptionRules {
    /// Maximum characters per line.
    pub max_chars: usize,
    /// Lines per caption (1 = single, 2 = double).
    pub lines: usize,
    /// Minimum caption duration; a short caption is extended into the silence after it, never
    /// over the next one.
    pub min_duration: Tick,
    /// Longest caption.
    pub max_duration: Tick,
    /// Frames left empty between consecutive captions.
    pub gap_frames: i64,
    /// A pause at least this long starts a new caption.
    pub break_pause: Tick,
}

impl Default for CaptionRules {
    fn default() -> Self {
        Self {
            max_chars: 42,
            lines: 2,
            min_duration: Tick(TICKS_PER_SECOND),
            max_duration: Tick(7 * TICKS_PER_SECOND),
            gap_frames: 0,
            break_pause: Tick(TICKS_PER_SECOND),
        }
    }
}

/// A caption block made from words.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptionBlock {
    pub start: Tick,
    pub end: Tick,
    /// Lines joined by `\n`.
    pub text: String,
    pub speaker: Option<String>,
    /// The word index range it shows.
    pub words: std::ops::Range<usize>,
}

/// Lay words out as caption blocks: words fill lines of at most `max_chars` (a longer single word
/// gets a line of its own) and blocks of `lines` lines; a new block starts at a speaker change,
/// at a pause of `break_pause`, after `max_duration`, or after sentence-ending punctuation once the
/// block is past half full. Times are snapped to frames, blocks never overlap and keep
/// `gap_frames` between them.
pub fn caption_blocks(words: &[SeqWord], rules: &CaptionRules, rate: FrameRate) -> Vec<CaptionBlock> {
    let max_chars = rules.max_chars.max(1);
    let max_lines = rules.lines.max(1);
    let cap_chars = max_chars * max_lines;
    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    let mut a = 0;
    let mut lines: Vec<usize> = vec![0];
    for i in 0..words.len() {
        let w = &words[i];
        let len = w.text.chars().count();
        if i > a {
            let prev = &words[i - 1];
            let cur = *lines.last().unwrap_or(&0);
            let fits_line = cur + 1 + len <= max_chars;
            let fits = fits_line || lines.len() < max_lines;
            let used: usize = lines.iter().sum::<usize>() + lines.len() - 1;
            let sentence_end = prev.text.ends_with(['.', '?', '!']) && used * 2 >= cap_chars;
            let brk =
                !fits || w.speaker != prev.speaker || w.start - prev.end >= rules.break_pause || w.end - words[a].start > rules.max_duration || sentence_end;
            if brk {
                groups.push(a..i);
                a = i;
                lines = vec![len];
                continue;
            }
            if fits_line {
                if let Some(l) = lines.last_mut() {
                    *l += 1 + len;
                }
            } else {
                lines.push(len);
            }
        } else {
            lines = vec![len];
        }
    }
    if a < words.len() {
        groups.push(a..words.len());
    }
    // text, frame-snapped times
    let fd = rate.frame_duration();
    let gap = Tick(fd.0 * rules.gap_frames.max(0));
    let mut out: Vec<CaptionBlock> = Vec::new();
    for g in groups {
        let mut text_lines: Vec<String> = vec![String::new()];
        for w in &words[g.clone()] {
            let Some(l) = text_lines.last_mut() else { break };
            if l.is_empty() {
                l.push_str(&w.text);
            } else if l.chars().count() + 1 + w.text.chars().count() <= max_chars {
                l.push(' ');
                l.push_str(&w.text);
            } else {
                text_lines.push(w.text.clone());
            }
        }
        let start = rate.snap(words[g.start].start);
        let mut end = rate.snap(words[g.end - 1].end);
        if end < words[g.end - 1].end {
            end += fd;
        }
        out.push(CaptionBlock { start, end: end.max(start + fd), text: text_lines.join("\n"), speaker: words[g.start].speaker.clone(), words: g });
    }
    // no overlaps, minimum duration (into the silence after a block, never over the next one)
    for i in 0..out.len() {
        if i > 0 {
            let min_start = out[i - 1].end + gap;
            if out[i].start < min_start {
                out[i].start = min_start;
                out[i].end = out[i].end.max(min_start + fd);
            }
        }
        let next = out.get(i + 1).map(|n| n.start);
        let b = &mut out[i];
        if b.end - b.start < rules.min_duration {
            let want = b.start + rules.min_duration;
            let mut e = rate.snap(want);
            if e < want {
                e += fd;
            }
            b.end = e;
        }
        if let Some(n) = next {
            b.end = b.end.min(n - gap).max(b.start + fd);
        }
    }
    out
}

/// Caption blocks as captions (ids from `ctx`).
pub fn blocks_to_captions(blocks: &[CaptionBlock], ctx: &mut EditCtx) -> Vec<Caption> {
    blocks
        .iter()
        .map(|b| Caption {
            id: ClipId(ctx.alloc()),
            start: b.start,
            duration: b.end - b.start,
            text: b.text.clone(),
            speaker: b.speaker.clone(),
            cue_id: None,
            settings: String::new(),
        })
        .collect()
}

/// Style mode for automatic captions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptionStyleMode {
    /// Full sentence or phrase (standard multi-word blocks).
    #[default]
    Sentence,
    /// Fast-paced, dynamic 1-3 word pops (viral short-form style: TikTok / Reels / Shorts).
    WordByWord,
    /// Full sentence on screen with the active spoken word highlighted.
    Karaoke,
}

impl CaptionStyleMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().replace(['-', '_', ' '], "").as_str() {
            "wordbyword" | "dynamic" | "pop" | "word" => Self::WordByWord,
            "karaoke" | "highlight" => Self::Karaoke,
            _ => Self::Sentence,
        }
    }
}

/// Caption visual preset / template.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptionPreset {
    /// Classic clean white text with subtle dark background box.
    Classic,
    /// Bold high-impact viral yellow (#FFE600) with dark outline (Hormozi style).
    #[default]
    ViralBold,
    /// High-energy MrBeast style: vibrant cyan (#00F0FF) / emerald (#22E574) with heavy outline.
    MrBeast,
    /// Minimalist modern white with thin outline and no background box.
    Minimalist,
    /// Cinematic warm tone (#F8E7C9), centered, elegant letter spacing.
    Cinematic,
}

impl CaptionPreset {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().replace(['-', '_', ' '], "").as_str() {
            "classic" | "default" | "subrip" => Self::Classic,
            "mrbeast" | "beast" | "punchy" => Self::MrBeast,
            "minimalist" | "minimal" | "clean" => Self::Minimalist,
            "cinematic" | "cinema" | "film" => Self::Cinematic,
            _ => Self::ViralBold,
        }
    }

    /// Default [`CaptionStyle`] associated with this preset.
    pub fn to_style(self, custom_color: Option<[u8; 4]>, anchor: Option<CaptionAnchor>) -> CaptionStyle {
        let anchor = anchor.unwrap_or(CaptionAnchor::Bottom);
        match self {
            Self::Classic => CaptionStyle {
                font: "Inter".into(),
                size: 52.0,
                color: custom_color.unwrap_or([255, 255, 255, 255]),
                background: true,
                background_color: [0, 0, 0, 190],
                align: CaptionAlign::Center,
                anchor,
                margin: 0.08,
                line_spacing: 1.2,
                outline: 0.0,
                outline_color: [0, 0, 0, 255],
            },
            Self::ViralBold => CaptionStyle {
                font: "Inter".into(),
                size: 64.0,
                color: custom_color.unwrap_or([255, 230, 0, 255]),
                background: false,
                background_color: [0, 0, 0, 0],
                align: CaptionAlign::Center,
                anchor,
                margin: 0.12,
                line_spacing: 1.15,
                outline: 4.5,
                outline_color: [0, 0, 0, 255],
            },
            Self::MrBeast => CaptionStyle {
                font: "Inter".into(),
                size: 68.0,
                color: custom_color.unwrap_or([0, 240, 255, 255]),
                background: false,
                background_color: [0, 0, 0, 0],
                align: CaptionAlign::Center,
                anchor,
                margin: 0.14,
                line_spacing: 1.15,
                outline: 5.0,
                outline_color: [0, 0, 0, 255],
            },
            Self::Minimalist => CaptionStyle {
                font: "Inter".into(),
                size: 46.0,
                color: custom_color.unwrap_or([255, 255, 255, 255]),
                background: false,
                background_color: [0, 0, 0, 0],
                align: CaptionAlign::Center,
                anchor,
                margin: 0.06,
                line_spacing: 1.25,
                outline: 2.0,
                outline_color: [0, 0, 0, 160],
            },
            Self::Cinematic => CaptionStyle {
                font: "Inter".into(),
                size: 44.0,
                color: custom_color.unwrap_or([248, 231, 201, 255]),
                background: false,
                background_color: [0, 0, 0, 0],
                align: CaptionAlign::Center,
                anchor,
                margin: 0.07,
                line_spacing: 1.35,
                outline: 1.5,
                outline_color: [10, 10, 10, 200],
            },
        }
    }
}

/// Options for automatic caption / subtitle generation.
#[derive(Clone, Debug, PartialEq)]
pub struct AutoCaptionOptions {
    pub style_mode: CaptionStyleMode,
    pub preset: CaptionPreset,
    pub uppercase: bool,
    pub auto_emoji: bool,
    pub remove_fillers: bool,
    /// Timing sync offset in sequence ticks (e.g. +100ms leads or delays caption display).
    pub sync_offset: Tick,
    pub lines: usize,
    pub max_chars: usize,
    pub anchor: Option<CaptionAnchor>,
    pub color: Option<[u8; 4]>,
}

impl Default for AutoCaptionOptions {
    fn default() -> Self {
        Self {
            style_mode: CaptionStyleMode::Sentence,
            preset: CaptionPreset::ViralBold,
            uppercase: true,
            auto_emoji: true,
            remove_fillers: true,
            sync_offset: Tick::ZERO,
            lines: 2,
            max_chars: 36,
            anchor: Some(CaptionAnchor::Bottom),
            color: None,
        }
    }
}

/// Adds contextual smart emojis based on spoken words and keywords (English and Portuguese).
pub fn add_smart_emoji(text: &str) -> String {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let mut emojis: Vec<&'static str> = Vec::new();
    for w in words {
        let emoji = match w {
            "dinheiro" | "grana" | "lucro" | "money" | "cash" | "faturar" | "rico" | "rich"
            | "milhao" | "milhão" | "milhoes" | "milhões" | "vendas" | "pix" | "faturamento"
            | "dolar" | "dólar" | "ouro" | "investir" | "renda" => Some("💰"),
            "fogo" | "quente" | "fire" | "viral" | "chama" | "hype" | "bombando" | "explodir"
            | "explosao" | "explosão" | "top" => Some("🔥"),
            "ideia" | "idea" | "segredo" | "dica" | "hack" | "insight" | "pensar" | "mentoria"
            | "aprender" | "segredos" | "truque" | "estrategia" | "estratégia" => Some("💡"),
            "foguete" | "rocket" | "crescer" | "escala" | "futuro" | "voar" | "topo" | "alturas"
            | "escalar" | "crescimento" | "avançar" => Some("🚀"),
            "atencao" | "atenção" | "cuidado" | "pare" | "wait" | "stop" | "alerta" | "perigo"
            | "urgente" | "erro" | "errado" | "aviso" => Some("⚠️"),
            "proibido" | "nunca" | "jamais" | "bloqueado" | "cancelado" | "perda" => Some("🚫"),
            "amor" | "coracao" | "coração" | "love" | "paixao" | "paixão" | "gostar" | "adorar"
            | "parabens" | "parabéns" => Some("❤️"),
            "sucesso" | "meta" | "alvo" | "win" | "vitoria" | "vitória" | "ganhar" | "campeao"
            | "campeão" | "vencer" | "resultado" | "foco" => Some("🎯"),
            "tempo" | "hora" | "minuto" | "segundo" | "time" | "rapido" | "rápido" | "tarde"
            | "relogio" | "relógio" | "cronometro" | "agora" | "ja" | "já" => Some("⏰"),
            "uau" | "wow" | "choque" | "loucura" | "surpresa" | "chocado" | "incrivel"
            | "incrível" | "inacreditavel" | "inacreditável" | "absurdo" => Some("😱"),
            "veja" | "olha" | "olhar" | "look" | "ver" | "olhe" | "repara" | "assista" | "repare" => Some("👀"),
            "alta" | "subir" | "ganho" | "estatistica" | "estatística" | "metrica" | "métrica" => Some("📈"),
            "forte" | "forca" | "força" | "disciplina" | "treino" | "gym" | "resiliencia" => Some("💪"),
            "estrela" | "destaque" | "especial" | "premium" | "vip" | "brilho" => Some("⭐"),
            _ => None,
        };
        if let Some(e) = emoji {
            if !emojis.contains(&e) {
                emojis.push(e);
            }
        }
    }
    if emojis.is_empty() {
        text.to_string()
    } else {
        format!("{text} {}", emojis.join(" "))
    }
}

/// Formats caption text with casing and optional smart emojis.
pub fn format_caption_text(raw: &str, uppercase: bool, auto_emoji: bool) -> String {
    let text = if uppercase {
        raw.trim().to_uppercase()
    } else {
        raw.trim().to_string()
    };
    if auto_emoji && !text.is_empty() {
        add_smart_emoji(&text)
    } else {
        text
    }
}

/// Generate automated captions from sequence words using modern viral / cinematic presets,
/// word-by-word dynamic popping, karaoke highlight, filler removal, smart emoji, and sync offset.
pub fn generate_auto_captions(
    words: &[SeqWord],
    opts: &AutoCaptionOptions,
    rate: FrameRate,
) -> (CaptionStyle, Vec<CaptionBlock>) {
    let style = opts.preset.to_style(opts.color, opts.anchor);
    if words.is_empty() {
        return (style, Vec::new());
    }

    // 1. Optionally remove filler words
    let working_words: Vec<SeqWord> = if opts.remove_fillers {
        let default_fillers: Vec<String> = DEFAULT_FILLERS.iter().map(|f| f.to_string()).collect();
        let hits = find_fillers(words, &default_fillers);
        let mut filtered = Vec::with_capacity(words.len());
        for (i, w) in words.iter().enumerate() {
            let is_filler = hits.iter().any(|range| range.contains(&i));
            if !is_filler {
                filtered.push(w.clone());
            }
        }
        if filtered.is_empty() {
            words.to_vec()
        } else {
            filtered
        }
    } else {
        words.to_vec()
    };

    if working_words.is_empty() {
        return (style, Vec::new());
    }

    let fd = rate.frame_duration();
    let min_dur = fd.max(Tick::from_seconds_f64(0.12));

    let mut blocks: Vec<CaptionBlock> = match opts.style_mode {
        CaptionStyleMode::WordByWord => {
            // Dynamic single/short word pop style
            let mut out = Vec::new();
            for (i, w) in working_words.iter().enumerate() {
                let text = format_caption_text(&w.text, opts.uppercase, opts.auto_emoji);
                if text.trim().is_empty() {
                    continue;
                }
                let raw_start = (w.start + opts.sync_offset).max(Tick::ZERO);
                let raw_end = (w.end + opts.sync_offset).max(raw_start + min_dur);
                let start = rate.snap(raw_start);
                let end = rate.snap(raw_end).max(start + fd);
                out.push(CaptionBlock {
                    start,
                    end,
                    text,
                    speaker: w.speaker.clone(),
                    words: i..i + 1,
                });
            }
            out
        }
        CaptionStyleMode::Karaoke => {
            // Group into sentence/pause chunks, then highlight active word
            let gap = Tick::from_seconds_f64(1.2);
            let paras = paragraphs(&working_words, gap);
            let mut out = Vec::new();
            for p in paras {
                let full_words: Vec<String> = working_words[p.clone()]
                    .iter()
                    .map(|w| w.text.trim().to_string())
                    .filter(|w| !w.is_empty())
                    .collect();
                if full_words.is_empty() {
                    continue;
                }
                for (active_idx, w_idx) in p.clone().enumerate() {
                    let w = &working_words[w_idx];
                    let active_word = w.text.trim();
                    if active_word.is_empty() {
                        continue;
                    }
                    let mut line = String::new();
                    for (i, word) in full_words.iter().enumerate() {
                        if i > 0 {
                            line.push(' ');
                        }
                        if i == active_idx {
                            line.push('[');
                            let formatted = format_caption_text(word, opts.uppercase, opts.auto_emoji);
                            line.push_str(&formatted);
                            line.push(']');
                        } else {
                            let formatted = if opts.uppercase {
                                word.to_uppercase()
                            } else {
                                word.clone()
                            };
                            line.push_str(&formatted);
                        }
                    }
                    let raw_start = (w.start + opts.sync_offset).max(Tick::ZERO);
                    let raw_end = (w.end + opts.sync_offset).max(raw_start + min_dur);
                    let start = rate.snap(raw_start);
                    let end = rate.snap(raw_end).max(start + fd);
                    out.push(CaptionBlock {
                        start,
                        end,
                        text: line,
                        speaker: w.speaker.clone(),
                        words: w_idx..w_idx + 1,
                    });
                }
            }
            out
        }
        CaptionStyleMode::Sentence => {
            let rules = CaptionRules {
                max_chars: opts.max_chars.max(10),
                lines: opts.lines.clamp(1, 4),
                min_duration: Tick::from_seconds_f64(1.0),
                max_duration: Tick::from_seconds_f64(6.0),
                gap_frames: 0,
                break_pause: Tick::from_seconds_f64(1.0),
            };
            let raw_blocks = caption_blocks(&working_words, &rules, rate);
            let mut out = Vec::with_capacity(raw_blocks.len());
            for b in raw_blocks {
                let text = format_caption_text(&b.text, opts.uppercase, opts.auto_emoji);
                if text.trim().is_empty() {
                    continue;
                }
                let raw_start = (b.start + opts.sync_offset).max(Tick::ZERO);
                let raw_end = (b.end + opts.sync_offset).max(raw_start + min_dur);
                let start = rate.snap(raw_start);
                let end = rate.snap(raw_end).max(start + fd);
                out.push(CaptionBlock {
                    start,
                    end,
                    text,
                    speaker: b.speaker,
                    words: b.words,
                });
            }
            out
        }
    };

    // Strict non-overlapping invariant enforcement
    blocks.retain(|b| !b.text.trim().is_empty());
    blocks.sort_by_key(|b| b.start);
    for i in 0..blocks.len() {
        if i > 0 {
            let prev_end = blocks[i - 1].end;
            if blocks[i].start < prev_end {
                blocks[i].start = prev_end;
            }
        }
        if blocks[i].end <= blocks[i].start {
            blocks[i].end = blocks[i].start + fd;
        }
    }
    for i in 0..blocks.len().saturating_sub(1) {
        let next_start = blocks[i + 1].start;
        if blocks[i].end > next_start {
            if next_start > blocks[i].start {
                blocks[i].end = next_start;
            } else {
                blocks[i + 1].start = blocks[i].start + fd;
                blocks[i + 1].end = blocks[i + 1].end.max(blocks[i + 1].start + fd);
                blocks[i].end = blocks[i + 1].start;
            }
        }
    }

    (style, blocks)
}

#[cfg(test)]
mod tests;
