//! Type-driven domain models for AniList library entries and local filtering.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::num::{NonZeroU8, NonZeroU32};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MediaId(pub NonZeroU32);

impl MediaId {
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for MediaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub NonZeroU32);

impl EntryId {
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LibraryStatus {
    #[serde(rename = "CURRENT")]
    Watching,
    #[serde(rename = "PLANNING")]
    Planning,
    #[serde(rename = "COMPLETED")]
    Completed,
    #[serde(rename = "DROPPED")]
    Dropped,
    #[serde(rename = "PAUSED")]
    Paused,
    #[serde(rename = "REPEATING")]
    Repeating,
}

impl LibraryStatus {
    pub const ALL: [Self; 6] = [
        Self::Watching,
        Self::Planning,
        Self::Completed,
        Self::Dropped,
        Self::Paused,
        Self::Repeating,
    ];

    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Watching => 0,
            Self::Planning => 1,
            Self::Completed => 2,
            Self::Dropped => 3,
            Self::Paused => 4,
            Self::Repeating => 5,
        }
    }

    #[must_use]
    pub const fn from_index(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(Self::Watching),
            1 => Some(Self::Planning),
            2 => Some(Self::Completed),
            3 => Some(Self::Dropped),
            4 => Some(Self::Paused),
            5 => Some(Self::Repeating),
            _ => None,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Watching => "Watching",
            Self::Planning => "Planning",
            Self::Completed => "Completed",
            Self::Dropped => "Dropped",
            Self::Paused => "Paused",
            Self::Repeating => "Repeating",
        }
    }
}

impl fmt::Display for LibraryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Normalized AniList score on the `1..=100` internal scale (`None` represents unrated / `0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScoreRaw(NonZeroU8);

impl ScoreRaw {
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        if raw >= 1 && raw <= 100 {
            match NonZeroU8::new(raw) {
                Some(nz) => Some(Self(nz)),
                None => None,
            }
        } else {
            None
        }
    }

    /// Converts a `score(format: POINT_100)` value from AniList (`0` or out-of-range maps to `None`).
    #[must_use]
    pub fn from_api_raw(raw: f64) -> Option<Self> {
        if !raw.is_finite() {
            return None;
        }
        let rounded = raw.round();
        if !(1.0..=100.0).contains(&rounded) {
            return None;
        }
        Self::new(rounded as u8)
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0.get()
    }
}

impl fmt::Display for ScoreRaw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ScoreFormat {
    #[default]
    #[serde(rename = "POINT_10_DECIMAL")]
    Point10Decimal,
    #[serde(rename = "POINT_100")]
    Point100,
    #[serde(rename = "POINT_10")]
    Point10,
    #[serde(rename = "POINT_5")]
    Point5,
    #[serde(rename = "POINT_3")]
    Point3,
}

impl ScoreFormat {
    pub const ALL: [Self; 5] = [
        Self::Point10Decimal,
        Self::Point100,
        Self::Point10,
        Self::Point5,
        Self::Point3,
    ];

    #[must_use]
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Point10Decimal => "point-10-decimal",
            Self::Point100 => "point-100",
            Self::Point10 => "point-10",
            Self::Point5 => "point-5",
            Self::Point3 => "point-3",
        }
    }

    #[must_use]
    pub fn from_config_key(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "point-10-decimal" | "point_10_decimal" | "decimal" | "10.0" => {
                Some(Self::Point10Decimal)
            }
            "point-100" | "point_100" | "100" => Some(Self::Point100),
            "point-10" | "point_10" | "10" => Some(Self::Point10),
            "point-5" | "point_5" | "5" | "stars" => Some(Self::Point5),
            "point-3" | "point_3" | "3" | "smiley" => Some(Self::Point3),
            _ => None,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Point10Decimal => "10.0 Decimal (9.8/10)",
            Self::Point100 => "100 Point (98/100)",
            Self::Point10 => "10 Point (9/10)",
            Self::Point5 => "5 Star (4★)",
            Self::Point3 => "3 Point (:))",
        }
    }

    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Point10Decimal => "9.8/10",
            Self::Point100 => "100pt",
            Self::Point10 => "10pt",
            Self::Point5 => "5★",
            Self::Point3 => "3pt",
        }
    }

    #[must_use]
    pub const fn uses_choice_picker(self) -> bool {
        matches!(self, Self::Point3 | Self::Point5)
    }

    #[must_use]
    pub const fn raw_to_point10(raw: ScoreRaw) -> u8 {
        let rounded = (raw.get() as u16 + 5) / 10;
        if rounded < 1 {
            1
        } else if rounded > 10 {
            10
        } else {
            rounded as u8
        }
    }

    /// Converts a `1..=100` score into a `1..=5` star rating using AniList's buckets.
    #[must_use]
    pub const fn raw_to_point5(raw: ScoreRaw) -> u8 {
        match raw.get() {
            1..=19 => 1,
            20..=39 => 2,
            40..=59 => 3,
            60..=79 => 4,
            _ => 5,
        }
    }

    /// Converts a `1..=5` star rating into AniList's canonical `scoreRaw` target.
    #[must_use]
    pub const fn point5_to_raw(stars: u8) -> Option<ScoreRaw> {
        match stars {
            1 => ScoreRaw::new(10),
            2 => ScoreRaw::new(30),
            3 => ScoreRaw::new(50),
            4 => ScoreRaw::new(70),
            5 => ScoreRaw::new(90),
            _ => None,
        }
    }

    #[must_use]
    pub fn format_table(self, score: Option<ScoreRaw>) -> Cow<'static, str> {
        let Some(raw) = score else {
            return Cow::Borrowed("·");
        };
        match self {
            Self::Point10Decimal => Cow::Owned(format!("{:.1}", f64::from(raw.get()) / 10.0)),
            Self::Point100 => Cow::Owned(raw.get().to_string()),
            Self::Point10 => Cow::Owned(Self::raw_to_point10(raw).to_string()),
            Self::Point5 => Cow::Owned(format!("{}★", Self::raw_to_point5(raw))),
            Self::Point3 => Cow::Borrowed(Point3Score::from_score_raw(raw).face()),
        }
    }

    #[must_use]
    pub fn format_badge(self, score: Option<ScoreRaw>) -> Cow<'static, str> {
        let Some(raw) = score else {
            return Cow::Borrowed("Rate");
        };
        match self {
            Self::Point10Decimal => Cow::Owned(format!("{:.1}/10", f64::from(raw.get()) / 10.0)),
            Self::Point100 => Cow::Owned(format!("{}/100", raw.get())),
            Self::Point10 => Cow::Owned(format!("{}/10", Self::raw_to_point10(raw))),
            Self::Point5 => Cow::Owned(format!("{}/5 ★", Self::raw_to_point5(raw))),
            Self::Point3 => Cow::Borrowed(Point3Score::from_score_raw(raw).face()),
        }
    }

    #[must_use]
    pub fn format_input_initial(self, score: Option<ScoreRaw>) -> String {
        let Some(raw) = score else {
            return String::new();
        };
        match self {
            Self::Point10Decimal => format!("{:.1}", f64::from(raw.get()) / 10.0),
            Self::Point100 => raw.get().to_string(),
            Self::Point10 => Self::raw_to_point10(raw).to_string(),
            Self::Point5 => Self::raw_to_point5(raw).to_string(),
            Self::Point3 => Point3Score::from_score_raw(raw).as_u8().to_string(),
        }
    }

    #[must_use]
    pub fn step_raw(self, current: Option<ScoreRaw>, delta_steps: i8) -> Option<ScoreRaw> {
        let step = i16::from(delta_steps);
        match self {
            Self::Point10Decimal | Self::Point100 => {
                let cur = current.map_or(0i16, |r| i16::from(r.get()));
                let next = (cur + step).clamp(0, 100) as u8;
                ScoreRaw::new(next)
            }
            Self::Point10 => {
                let cur = current.map_or(0i16, |r| i16::from(Self::raw_to_point10(r)));
                let next = (cur + step).clamp(0, 10) as u8;
                ScoreRaw::new(next.saturating_mul(10))
            }
            Self::Point5 => {
                let cur = current.map_or(0i16, |r| i16::from(Self::raw_to_point5(r)));
                let next = (cur + step).clamp(0, 5) as u8;
                Self::point5_to_raw(next)
            }
            Self::Point3 => {
                let cur = current.map_or(0i16, |r| i16::from(Point3Score::from_score_raw(r).as_u8()));
                let next = (cur + step).clamp(0, 3) as u8;
                match next {
                    1 => Some(Point3Score::Sad.as_score_raw()),
                    2 => Some(Point3Score::Neutral.as_score_raw()),
                    3 => Some(Point3Score::Happy.as_score_raw()),
                    _ => None,
                }
            }
        }
    }

    pub fn parse_input(self, raw_input: &str) -> Result<ScoreUpdate, &'static str> {
        let s = raw_input.trim();
        if s.is_empty() {
            return Ok(ScoreUpdate::Clear);
        }

        match self {
            Self::Point10Decimal => {
                let val: f64 = s
                    .parse()
                    .map_err(|_| "Enter a score between 0.0 and 10.0")?;
                if !val.is_finite() || !(0.0..=10.0).contains(&val) {
                    return Err("Score must be between 0.0 and 10.0");
                }
                let scaled = (val * 10.0).round() as u8;
                match ScoreRaw::new(scaled) {
                    Some(raw) => Ok(ScoreUpdate::Set(raw)),
                    None => Ok(ScoreUpdate::Clear),
                }
            }
            Self::Point100 => {
                let val: u16 = s
                    .parse()
                    .map_err(|_| "Enter a whole number between 0 and 100")?;
                if val > 100 {
                    return Err("Score must be between 0 and 100");
                }
                match ScoreRaw::new(val as u8) {
                    Some(raw) => Ok(ScoreUpdate::Set(raw)),
                    None => Ok(ScoreUpdate::Clear),
                }
            }
            Self::Point10 => {
                let val: u16 = s
                    .parse()
                    .map_err(|_| "Enter a whole number between 0 and 10")?;
                if val > 10 {
                    return Err("Score must be between 0 and 10");
                }
                match ScoreRaw::new((val as u8).saturating_mul(10)) {
                    Some(raw) => Ok(ScoreUpdate::Set(raw)),
                    None => Ok(ScoreUpdate::Clear),
                }
            }
            Self::Point5 => {
                let val: u8 = s
                    .parse()
                    .map_err(|_| "Enter a star rating between 0 and 5")?;
                if val > 5 {
                    return Err("Score must be between 0 and 5");
                }
                match Self::point5_to_raw(val) {
                    Some(raw) => Ok(ScoreUpdate::Set(raw)),
                    None => Ok(ScoreUpdate::Clear),
                }
            }
            Self::Point3 => {
                let val: u8 = s.parse().map_err(|_| "Enter 0, 1, 2, or 3")?;
                match val {
                    0 => Ok(ScoreUpdate::Clear),
                    1 => Ok(ScoreUpdate::Set(Point3Score::Sad.as_score_raw())),
                    2 => Ok(ScoreUpdate::Set(Point3Score::Neutral.as_score_raw())),
                    3 => Ok(ScoreUpdate::Set(Point3Score::Happy.as_score_raw())),
                    _ => Err("Enter 0, 1, 2, or 3"),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Point3Score {
    Sad = 1,
    Neutral = 2,
    Happy = 3,
}

impl Point3Score {
    pub const ALL: [Self; 3] = [Self::Happy, Self::Neutral, Self::Sad];

    #[must_use]
    pub const fn from_score_raw(raw: ScoreRaw) -> Self {
        match raw.get() {
            1..=47 => Self::Sad,
            48..=72 => Self::Neutral,
            _ => Self::Happy,
        }
    }

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Normalized `1..=100` [`ScoreRaw`] expected by `SaveMediaListEntry(scoreRaw: ...)`.
    #[must_use]
    pub const fn as_score_raw(self) -> ScoreRaw {
        let raw = match self {
            Self::Sad => 35,
            Self::Neutral => 60,
            Self::Happy => 85,
        };
        match ScoreRaw::new(raw) {
            Some(s) => s,
            None => unreachable!(),
        }
    }

    #[must_use]
    pub const fn face(self) -> &'static str {
        match self {
            Self::Sad => ":(",
            Self::Neutral => ":|",
            Self::Happy => ":)",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sad => ":(  Disliked",
            Self::Neutral => ":|  Neutral",
            Self::Happy => ":)  Liked",
        }
    }
}

impl fmt::Display for Point3Score {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.face())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreUpdate {
    Clear,
    Set(ScoreRaw),
}

impl ScoreUpdate {
    #[must_use]
    pub const fn to_score_raw(self) -> u8 {
        match self {
            Self::Clear => 0,
            Self::Set(score) => score.get(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SortDirection {
    Asc,
    #[default]
    Desc,
}

impl SortDirection {
    #[must_use]
    pub const fn toggle(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    #[must_use]
    pub const fn arrow(self) -> &'static str {
        match self {
            Self::Asc => "↑",
            Self::Desc => "↓",
        }
    }

    #[must_use]
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    #[must_use]
    pub fn from_config_key(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "asc" | "ascending" => Some(Self::Asc),
            "desc" | "descending" => Some(Self::Desc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SortField {
    #[default]
    Updated,
    Title,
    Score,
    AverageScore,
    Progress,
}

impl SortField {
    pub const ALL: [Self; 5] = [
        Self::Updated,
        Self::Title,
        Self::Score,
        Self::AverageScore,
        Self::Progress,
    ];

    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Updated => 0,
            Self::Title => 1,
            Self::Score => 2,
            Self::AverageScore => 3,
            Self::Progress => 4,
        }
    }

    #[must_use]
    pub const fn from_index(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(Self::Updated),
            1 => Some(Self::Title),
            2 => Some(Self::Score),
            3 => Some(Self::AverageScore),
            4 => Some(Self::Progress),
            _ => None,
        }
    }

    #[must_use]
    pub const fn default_direction(self) -> SortDirection {
        match self {
            Self::Title => SortDirection::Asc,
            Self::Updated | Self::Score | Self::AverageScore | Self::Progress => {
                SortDirection::Desc
            }
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Updated => "Recently Updated",
            Self::Title => "Title",
            Self::Score => "My Rating",
            Self::AverageScore => "Average Score",
            Self::Progress => "Episode Progress",
        }
    }

    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Updated => "Updated",
            Self::Title => "Title",
            Self::Score => "Rating",
            Self::AverageScore => "Avg",
            Self::Progress => "Progress",
        }
    }

    #[must_use]
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Title => "title",
            Self::Score => "score",
            Self::AverageScore => "average-score",
            Self::Progress => "progress",
        }
    }

    #[must_use]
    pub fn from_config_key(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "updated" | "default" | "recent" => Some(Self::Updated),
            "title" | "name" | "alphabetical" => Some(Self::Title),
            "score" | "rating" => Some(Self::Score),
            "average-score" | "average_score" | "avg" | "average" => Some(Self::AverageScore),
            "progress" | "episodes" => Some(Self::Progress),
            _ => None,
        }
    }

    /// Unrated / missing scores (`None`) are always placed after present values so
    /// ascending score sorts surface the lowest rated titles first instead of unrated ones.
    #[must_use]
    pub fn compare_items(self, a: &AnimeItem, b: &AnimeItem, direction: SortDirection) -> Ordering {
        let title_tiebreak = || {
            cmp_case_insensitive(&a.title, &b.title)
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.id.get().cmp(&b.id.get()))
        };

        match self {
            Self::Updated => title_tiebreak(),
            Self::Title => {
                let ord = cmp_case_insensitive(&a.title, &b.title)
                    .then_with(|| a.title.cmp(&b.title));
                let directed = match direction {
                    SortDirection::Asc => ord,
                    SortDirection::Desc => ord.reverse(),
                };
                directed.then_with(|| a.id.get().cmp(&b.id.get()))
            }
            Self::Score => compare_optional_none_last(a.entry.score, b.entry.score, direction)
                .then_with(title_tiebreak),
            Self::AverageScore => {
                compare_optional_none_last(a.average_score, b.average_score, direction)
                    .then_with(title_tiebreak)
            }
            Self::Progress => {
                let ord = a.entry.progress.cmp(&b.entry.progress);
                let directed = match direction {
                    SortDirection::Asc => ord,
                    SortDirection::Desc => ord.reverse(),
                };
                directed.then_with(title_tiebreak)
            }
        }
    }
}

fn compare_optional_none_last<T: Ord>(
    a: Option<T>,
    b: Option<T>,
    direction: SortDirection,
) -> Ordering {
    match (a, b) {
        (Some(va), Some(vb)) => match direction {
            SortDirection::Asc => va.cmp(&vb),
            SortDirection::Desc => vb.cmp(&va),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_case_insensitive(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        for (ba, bb) in a.bytes().zip(b.bytes()) {
            let ord = ba.to_ascii_lowercase().cmp(&bb.to_ascii_lowercase());
            if ord != Ordering::Equal {
                return ord;
            }
        }
        return a.len().cmp(&b.len());
    }

    let mut ca = a.chars().flat_map(char::to_lowercase);
    let mut cb = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ca.next(), cb.next()) {
            (Some(la), Some(lb)) => {
                let ord = la.cmp(&lb);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub id: Option<EntryId>,
    pub status: LibraryStatus,
    pub progress: u32,
    pub score: Option<ScoreRaw>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnimeItem {
    pub id: MediaId,
    pub title: String,
    #[serde(default)]
    pub alt_title: Option<String>,
    pub episodes: Option<u32>,
    pub release_status: String,
    pub average_score: Option<u8>,
    pub genres: Vec<String>,
    pub description_text: String,
    pub cover_url: Option<String>,
    pub site_url: Option<String>,
    pub entry: LibraryEntry,
    #[serde(skip)]
    search_haystack: String,
}

impl AnimeItem {
    #[allow(
        clippy::too_many_arguments,
        reason = "constructor initializes all normalized fields and precomputes the search index"
    )]
    #[must_use]
    pub fn new(
        id: MediaId,
        title: String,
        alt_title: Option<String>,
        episodes: Option<u32>,
        release_status: String,
        average_score: Option<u8>,
        genres: Vec<String>,
        description_text: String,
        cover_url: Option<String>,
        site_url: Option<String>,
        entry: LibraryEntry,
    ) -> Self {
        let mut item = Self {
            id,
            title,
            alt_title,
            episodes,
            release_status,
            average_score,
            genres,
            description_text,
            cover_url,
            site_url,
            entry,
            search_haystack: String::new(),
        };
        item.rebuild_search_index();
        item
    }

    pub fn rebuild_search_index(&mut self) {
        let genres_len: usize = self.genres.iter().map(|g| g.len() + 1).sum();
        let cap = self.title.len()
            + 1
            + self.alt_title.as_ref().map_or(0, |a| a.len() + 1)
            + genres_len;
        self.search_haystack.clear();
        self.search_haystack.reserve(cap);

        push_lowercased(&self.title, &mut self.search_haystack);
        if let Some(alt) = &self.alt_title {
            self.search_haystack.push('\n');
            push_lowercased(alt, &mut self.search_haystack);
        }
        for genre in &self.genres {
            self.search_haystack.push('\n');
            push_lowercased(genre, &mut self.search_haystack);
        }
    }

    #[must_use]
    pub fn clamp_progress(&self, next: u32) -> u32 {
        match self.episodes {
            Some(total) => next.min(total),
            None => next,
        }
    }

    #[must_use]
    pub fn progress_display(&self) -> String {
        match self.episodes {
            Some(total) => format!("Ep {} / {total}", self.entry.progress),
            None => format!("Ep {} / ?", self.entry.progress),
        }
    }

    #[must_use]
    pub fn release_status_display(&self) -> Cow<'static, str> {
        match self.release_status.as_str() {
            "FINISHED" => return Cow::Borrowed("Finished"),
            "RELEASING" => return Cow::Borrowed("Releasing"),
            "NOT_YET_RELEASED" => return Cow::Borrowed("Not Yet Released"),
            "CANCELLED" => return Cow::Borrowed("Cancelled"),
            "HIATUS" => return Cow::Borrowed("Hiatus"),
            "UNKNOWN" | "" => return Cow::Borrowed("Unknown"),
            _ => {}
        }

        let mut out = String::with_capacity(self.release_status.len());
        for (idx, word) in self
            .release_status
            .split('_')
            .filter(|w| !w.is_empty())
            .enumerate()
        {
            if idx > 0 {
                out.push(' ');
            }
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                for up in first.to_uppercase() {
                    out.push(up);
                }
                for rest in chars {
                    for low in rest.to_lowercase() {
                        out.push(low);
                    }
                }
            }
        }
        if out.is_empty() {
            Cow::Borrowed("Unknown")
        } else {
            Cow::Owned(out)
        }
    }

    #[must_use]
    pub fn matches_normalized(&self, lowercase_query: &str) -> bool {
        if lowercase_query.is_empty() {
            return true;
        }
        self.search_haystack.contains(lowercase_query)
    }

    #[must_use]
    pub fn matches_filter(&self, query: &str) -> bool {
        let needle = query.trim();
        if needle.is_empty() {
            return true;
        }
        let lowercase_query = needle.to_lowercase();
        self.matches_normalized(&lowercase_query)
    }
}

fn push_lowercased(input: &str, dst: &mut String) {
    if input.is_ascii() {
        for b in input.bytes() {
            dst.push(b.to_ascii_lowercase() as char);
        }
    } else {
        for ch in input.chars() {
            for low in ch.to_lowercase() {
                dst.push(low);
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryCollection {
    lists: [Vec<AnimeItem>; 6],
    #[serde(default)]
    pub score_format: Option<ScoreFormat>,
}

impl LibraryCollection {
    #[must_use]
    pub fn from_lists(lists: HashMap<LibraryStatus, Vec<AnimeItem>>) -> Self {
        let mut arr: [Vec<AnimeItem>; 6] = Default::default();
        for (status, mut items) in lists {
            for item in &mut items {
                item.rebuild_search_index();
            }
            arr[status.index()] = items;
        }
        Self {
            lists: arr,
            score_format: None,
        }
    }

    #[must_use]
    pub fn from_array(mut lists: [Vec<AnimeItem>; 6], score_format: Option<ScoreFormat>) -> Self {
        for bucket in &mut lists {
            for item in bucket {
                item.rebuild_search_index();
            }
        }
        Self {
            lists,
            score_format,
        }
    }

    pub fn rebuild_search_indexes(&mut self) {
        for bucket in &mut self.lists {
            for item in bucket {
                item.rebuild_search_index();
            }
        }
    }

    #[must_use]
    pub fn items_for(&self, status: LibraryStatus) -> &[AnimeItem] {
        &self.lists[status.index()]
    }

    pub fn items_for_mut(&mut self, status: LibraryStatus) -> &mut Vec<AnimeItem> {
        &mut self.lists[status.index()]
    }

    #[must_use]
    pub fn count_for(&self, status: LibraryStatus) -> usize {
        self.lists[status.index()].len()
    }

    #[must_use]
    pub fn total_count(&self) -> usize {
        self.lists.iter().map(Vec::len).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lists.iter().all(Vec::is_empty)
    }

    #[must_use]
    pub fn find_by_media_id(&self, media_id: MediaId) -> Option<(LibraryStatus, usize)> {
        for status in LibraryStatus::ALL {
            if let Some(pos) = self.lists[status.index()]
                .iter()
                .position(|item| item.id == media_id)
            {
                return Some((status, pos));
            }
        }
        None
    }

    #[must_use]
    pub fn get(&self, media_id: MediaId) -> Option<&AnimeItem> {
        let (status, idx) = self.find_by_media_id(media_id)?;
        self.lists[status.index()].get(idx)
    }

    pub fn get_mut(&mut self, media_id: MediaId) -> Option<&mut AnimeItem> {
        let (status, idx) = self.find_by_media_id(media_id)?;
        self.lists[status.index()].get_mut(idx)
    }

    pub fn move_item(&mut self, media_id: MediaId, from: LibraryStatus, to: LibraryStatus) -> bool {
        if from == to {
            return false;
        }
        let src = &mut self.lists[from.index()];
        let Some(pos) = src.iter().position(|item| item.id == media_id) else {
            return false;
        };
        let mut item = src.remove(pos);
        item.entry.status = to;
        self.lists[to.index()].insert(0, item);
        true
    }

    pub fn remove_item(&mut self, media_id: MediaId) -> Option<(LibraryStatus, usize, AnimeItem)> {
        let (status, idx) = self.find_by_media_id(media_id)?;
        let item = self.lists[status.index()].remove(idx);
        Some((status, idx, item))
    }

    pub fn restore_item(&mut self, status: LibraryStatus, index: usize, mut item: AnimeItem) {
        item.entry.status = status;
        let bucket = &mut self.lists[status.index()];
        let insert_at = index.min(bucket.len());
        bucket.insert(insert_at, item);
    }
}

/// Strips HTML tags and decodes common HTML entities in a single pass.
#[must_use]
pub fn clean_description(raw: Option<&str>) -> String {
    let Some(input) = raw else {
        return "No description available.".to_owned();
    };

    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut newline_run = 0u8;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(tag_len) = find_html_tag_len(&bytes[i..]) {
                let tag_body = &input[i + 1..i + tag_len - 1];
                if is_br_tag(tag_body) {
                    push_newline(&mut out, &mut newline_run);
                }
                i += tag_len;
                continue;
            }
        }

        if bytes[i] == b'&' {
            if let Some((decoded, consumed)) = decode_html_entity(&input[i..]) {
                if decoded == '\n' {
                    push_newline(&mut out, &mut newline_run);
                } else {
                    newline_run = 0;
                    out.push(decoded);
                }
                i += consumed;
                continue;
            }
        }

        let ch = input[i..]
            .chars()
            .next()
            .unwrap_or('\u{FFFD}');
        match ch {
            '\r' => {}
            '\n' => push_newline(&mut out, &mut newline_run),
            _ => {
                newline_run = 0;
                out.push(ch);
            }
        }
        i += ch.len_utf8();
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        "No description available.".to_owned()
    } else if trimmed.len() == out.len() {
        out
    } else {
        trimmed.to_owned()
    }
}

fn push_newline(out: &mut String, newline_run: &mut u8) {
    if *newline_run < 2 && !out.is_empty() {
        out.push('\n');
        *newline_run += 1;
    }
}

fn find_html_tag_len(slice: &[u8]) -> Option<usize> {
    if slice.len() < 3 {
        return None;
    }
    let second = slice[1];
    if !(second.is_ascii_alphabetic() || second == b'/' || second == b'!') {
        return None;
    }
    let limit = slice.len().min(256);
    for (idx, &b) in slice.iter().enumerate().take(limit).skip(1) {
        if b == b'<' {
            return None;
        }
        if b == b'>' {
            return Some(idx + 1);
        }
    }
    None
}

fn is_br_tag(tag_body: &str) -> bool {
    let trimmed = tag_body.trim();
    if trimmed.eq_ignore_ascii_case("br") || trimmed.eq_ignore_ascii_case("br/") {
        return true;
    }
    let bytes = trimmed.as_bytes();
    bytes.len() > 2
        && bytes[..2].eq_ignore_ascii_case(b"br")
        && (bytes[2].is_ascii_whitespace() || bytes[2] == b'/')
}

fn decode_html_entity(slice: &str) -> Option<(char, usize)> {
    const ENTITIES: &[(&str, char)] = &[
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&#39;", '\''),
        ("&#039;", '\''),
        ("&apos;", '\''),
        ("&nbsp;", ' '),
        ("&mdash;", '—'),
        ("&ndash;", '–'),
        ("&hellip;", '…'),
    ];
    for &(pat, ch) in ENTITIES {
        if slice.starts_with(pat) {
            return Some((ch, pat.len()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item(
        id: u32,
        title: &str,
        alt_title: Option<&str>,
        genres: &[&str],
        episodes: Option<u32>,
    ) -> AnimeItem {
        AnimeItem::new(
            MediaId(NonZeroU32::new(id).unwrap()),
            title.to_owned(),
            alt_title.map(str::to_owned),
            episodes,
            "FINISHED".to_owned(),
            Some(85),
            genres.iter().map(|s| (*s).to_owned()).collect(),
            "Sample".to_owned(),
            None,
            None,
            LibraryEntry {
                id: NonZeroU32::new(id + 1000).map(EntryId),
                status: LibraryStatus::Watching,
                progress: 3,
                score: Some(Point3Score::Happy.as_score_raw()),
            },
        )
    }

    #[test]
    fn score_raw_and_all_score_formats_convert_and_format_accurately() {
        assert_eq!(ScoreRaw::new(0), None);
        assert_eq!(ScoreRaw::new(101), None);
        assert_eq!(ScoreRaw::from_api_raw(0.0), None);
        assert_eq!(ScoreRaw::from_api_raw(98.0).map(ScoreRaw::get), Some(98));

        let s98 = ScoreRaw::new(98).unwrap();
        assert_eq!(ScoreFormat::default(), ScoreFormat::Point10Decimal);
        assert_eq!(ScoreFormat::Point10Decimal.format_table(Some(s98)), "9.8");
        assert_eq!(
            ScoreFormat::Point10Decimal.format_badge(Some(s98)),
            "9.8/10"
        );
        assert_eq!(ScoreFormat::Point100.format_table(Some(s98)), "98");
        assert_eq!(ScoreFormat::Point100.format_badge(Some(s98)), "98/100");
        assert_eq!(ScoreFormat::Point10.format_table(Some(s98)), "10");
        assert_eq!(ScoreFormat::Point5.format_table(Some(s98)), "5★");
        assert_eq!(ScoreFormat::Point3.format_table(Some(s98)), ":)");

        assert_eq!(
            ScoreFormat::Point10Decimal.parse_input("9.8"),
            Ok(ScoreUpdate::Set(s98))
        );
        assert_eq!(
            ScoreFormat::Point10Decimal.parse_input("0"),
            Ok(ScoreUpdate::Clear)
        );
        assert!(ScoreFormat::Point10Decimal.parse_input("10.5").is_err());
        assert_eq!(
            ScoreFormat::Point5.parse_input("4"),
            Ok(ScoreUpdate::Set(ScoreRaw::new(70).unwrap()))
        );
        assert_eq!(
            ScoreFormat::Point10.parse_input("9"),
            Ok(ScoreUpdate::Set(ScoreRaw::new(90).unwrap()))
        );

        assert_eq!(Point3Score::Sad.as_score_raw().get(), 35);
        assert_eq!(Point3Score::Neutral.as_score_raw().get(), 60);
        assert_eq!(Point3Score::Happy.as_score_raw().get(), 85);

        assert_eq!(ScoreUpdate::Clear.to_score_raw(), 0);
        assert_eq!(
            ScoreUpdate::Set(Point3Score::Sad.as_score_raw()).to_score_raw(),
            35
        );
        assert_eq!(
            ScoreUpdate::Set(Point3Score::Neutral.as_score_raw()).to_score_raw(),
            60
        );
        assert_eq!(
            ScoreUpdate::Set(Point3Score::Happy.as_score_raw()).to_score_raw(),
            85
        );
    }

    #[test]
    fn status_index_roundtrips_all_six_statuses() {
        for (idx, status) in LibraryStatus::ALL.into_iter().enumerate() {
            assert_eq!(status.index(), idx);
            assert_eq!(LibraryStatus::from_index(idx), Some(status));
        }
        assert_eq!(LibraryStatus::from_index(6), None);
    }

    #[test]
    fn clamps_episode_progress_when_total_is_known() {
        let bounded = sample_item(1, "Frieren", None, &["Fantasy"], Some(28));
        assert_eq!(bounded.clamp_progress(35), 28);
        assert_eq!(bounded.clamp_progress(12), 12);

        let unbounded = sample_item(2, "One Piece", None, &["Action"], None);
        assert_eq!(unbounded.clamp_progress(1100), 1100);
    }

    #[test]
    fn local_filter_matches_title_alt_title_and_genres() {
        let frieren = sample_item(
            1,
            "Frieren: Beyond Journey's End",
            Some("Sousou no Frieren"),
            &["Adventure", "Drama"],
            Some(28),
        );
        let bocchi = sample_item(
            2,
            "Bocchi the Rock!",
            None,
            &["Comedy", "Music"],
            Some(12),
        );
        let steins = sample_item(
            3,
            "Steins;Gate",
            None,
            &["Sci-Fi", "Thriller"],
            Some(24),
        );

        assert!(frieren.matches_filter(""));
        assert!(frieren.matches_filter("sousou"));
        assert!(!frieren.matches_filter("music"));
        assert!(bocchi.matches_filter("MUSIC"));
        assert!(steins.matches_filter("sci-fi"));
    }

    #[test]
    fn library_move_remove_and_restore_preserve_invariants() {
        let item1 = sample_item(10, "Show A", None, &["Action"], Some(12));
        let item2 = sample_item(20, "Show B", None, &["Drama"], Some(24));
        let mut lists = HashMap::new();
        lists.insert(LibraryStatus::Watching, vec![item1, item2]);

        let mut lib = LibraryCollection::from_lists(lists);
        let id1 = MediaId(NonZeroU32::new(10).unwrap());
        let id2 = MediaId(NonZeroU32::new(20).unwrap());

        assert!(lib.move_item(id1, LibraryStatus::Watching, LibraryStatus::Completed));
        assert_eq!(lib.count_for(LibraryStatus::Watching), 1);
        assert_eq!(lib.count_for(LibraryStatus::Completed), 1);
        assert_eq!(
            lib.get(id1).unwrap().entry.status,
            LibraryStatus::Completed
        );

        let (prev_status, prev_idx, removed) = lib.remove_item(id2).unwrap();
        assert_eq!(prev_status, LibraryStatus::Watching);
        assert_eq!(prev_idx, 0);
        assert_eq!(lib.count_for(LibraryStatus::Watching), 0);

        lib.restore_item(prev_status, prev_idx, removed);
        assert_eq!(lib.count_for(LibraryStatus::Watching), 1);
        assert_eq!(lib.items_for(LibraryStatus::Watching)[0].id, id2);
    }

    #[test]
    fn clean_description_single_pass_handles_tags_entities_and_edge_cases() {
        let raw = Some("Line 1<br><i>Italic &amp; Bold</i> &quot;Quote&quot;");
        assert_eq!(clean_description(raw), "Line 1\nItalic & Bold \"Quote\"");
        assert_eq!(clean_description(None), "No description available.");
        assert_eq!(clean_description(Some("   <br>  ")), "No description available.");
        assert_eq!(
            clean_description(Some(
                "<br/>A<br /><BR><br><br>B &mdash; C &ndash; D&hellip; <3 &#039;ok&#39;"
            )),
            "A\n\nB — C – D… <3 'ok'"
        );
    }

    #[test]
    fn release_status_display_formats_underscored_enums() {
        let mut item = sample_item(5, "Test", None, &[], None);
        item.release_status = "NOT_YET_RELEASED".to_owned();
        assert_eq!(item.release_status_display(), "Not Yet Released");
        item.release_status = "RELEASING".to_owned();
        assert_eq!(item.release_status_display(), "Releasing");
    }

    #[test]
    fn sort_field_compares_title_score_avg_and_progress_deterministically() {
        let mut a = sample_item(1, "beta", None, &[], Some(12));
        a.entry.score = ScoreRaw::new(80);
        a.average_score = Some(75);
        a.entry.progress = 10;

        let mut b = sample_item(2, "Alpha", None, &[], Some(24));
        b.entry.score = None;
        b.average_score = Some(92);
        b.entry.progress = 3;

        let mut c = sample_item(3, "Gamma", None, &[], Some(12));
        c.entry.score = ScoreRaw::new(95);
        c.average_score = None;
        c.entry.progress = 12;

        // Title Asc (case-insensitive): Alpha < beta < Gamma
        assert_eq!(
            SortField::Title.compare_items(&b, &a, SortDirection::Asc),
            Ordering::Less
        );
        assert_eq!(
            SortField::Title.compare_items(&a, &c, SortDirection::Asc),
            Ordering::Less
        );
        // Title Desc: Gamma < beta < Alpha
        assert_eq!(
            SortField::Title.compare_items(&c, &a, SortDirection::Desc),
            Ordering::Less
        );

        // Score Desc: 95 (c) < 80 (a) < None (b)
        assert_eq!(
            SortField::Score.compare_items(&c, &a, SortDirection::Desc),
            Ordering::Less
        );
        assert_eq!(
            SortField::Score.compare_items(&a, &b, SortDirection::Desc),
            Ordering::Less
        );
        // Score Asc: 80 (a) < 95 (c) < None (b) — unrated stays last
        assert_eq!(
            SortField::Score.compare_items(&a, &c, SortDirection::Asc),
            Ordering::Less
        );
        assert_eq!(
            SortField::Score.compare_items(&c, &b, SortDirection::Asc),
            Ordering::Less
        );

        // AverageScore Desc: 92 (b) < 75 (a) < None (c)
        assert_eq!(
            SortField::AverageScore.compare_items(&b, &a, SortDirection::Desc),
            Ordering::Less
        );
        assert_eq!(
            SortField::AverageScore.compare_items(&a, &c, SortDirection::Desc),
            Ordering::Less
        );

        // Progress Desc: 12 (c) < 10 (a) < 3 (b)
        assert_eq!(
            SortField::Progress.compare_items(&c, &a, SortDirection::Desc),
            Ordering::Less
        );
        assert_eq!(
            SortField::Progress.compare_items(&a, &b, SortDirection::Desc),
            Ordering::Less
        );
    }
}
