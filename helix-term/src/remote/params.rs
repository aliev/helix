use serde::Deserialize;
use helix_view::tree;

#[derive(Debug, Clone, Copy)]
pub enum SplitDirection {
    Left,
    Right,
    Up,
    Down,
}

impl SplitDirection {
    pub fn focus_direction(self) -> tree::Direction {
        match self {
            Self::Left => tree::Direction::Left,
            Self::Right => tree::Direction::Right,
            Self::Up => tree::Direction::Up,
            Self::Down => tree::Direction::Down,
        }
    }
}

impl<'de> Deserialize<'de> for SplitDirection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            "horizontal" => Ok(Self::Down),
            "vertical" => Ok(Self::Right),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["left", "right", "up", "down", "horizontal", "vertical"],
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenFileArgs {
    pub path: String,
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub line: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub column: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct SplitOpenArgs {
    pub path: String,
    pub cwd: Option<String>,
    pub direction: SplitDirection,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub line: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub column: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct FocusSplitArgs {
    pub direction: SplitDirection,
}

#[derive(Debug, Deserialize)]
pub struct GotoLocationArgs {
    pub path: Option<String>,
    pub cwd: Option<String>,
    #[serde(deserialize_with = "deserialize_usizeish")]
    pub line: usize,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub column: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct SelectLinesArgs {
    pub path: Option<String>,
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub line: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub start_line: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub end_line: Option<usize>,
}

impl SelectLinesArgs {
    pub fn resolved_start_line(&self) -> Option<usize> {
        self.start_line.or(self.line)
    }
}

#[derive(Debug, Deserialize)]
pub struct McpPresenceArgs {
    pub client_id: String,
    pub client_name: String,
}

#[derive(Debug, Deserialize)]
pub struct GetDiagnosticsArgs {
    pub path: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetCurrentDocumentArgs {
    pub path: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetSelectionsArgs {
    pub path: Option<String>,
    pub cwd: Option<String>,
}

fn deserialize_usizeish<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Usizeish {
        Number(usize),
        String(String),
    }

    match Usizeish::deserialize(deserializer)? {
        Usizeish::Number(value) => Ok(value),
        Usizeish::String(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

fn deserialize_optional_usizeish<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OptionalUsizeish {
        Number(usize),
        String(String),
        Null,
    }

    match OptionalUsizeish::deserialize(deserializer)? {
        OptionalUsizeish::Number(value) => Ok(Some(value)),
        OptionalUsizeish::String(value) => {
            value.parse().map(Some).map_err(serde::de::Error::custom)
        }
        OptionalUsizeish::Null => Ok(None),
    }
}
