use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct OpenFileArgs {
    pub path: String,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub line: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub column: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct GotoLocationArgs {
    pub path: Option<String>,
    #[serde(deserialize_with = "deserialize_usizeish")]
    pub line: usize,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub column: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct SelectLinesArgs {
    pub path: Option<String>,
    #[serde(deserialize_with = "deserialize_usizeish")]
    pub start_line: usize,
    #[serde(default, deserialize_with = "deserialize_optional_usizeish")]
    pub end_line: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct McpPresenceArgs {
    pub client_id: String,
    pub client_name: String,
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
