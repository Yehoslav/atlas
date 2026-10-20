use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};

/// The dashboard's only path to media and media facts. It has no store and no
/// import from the Atlas crate: even in this repository, the consumer crosses
/// the same HTTP boundary any other site would.
#[derive(Clone)]
pub struct Atlas {
    base_url: String,
    client: Client,
}

impl Atlas {
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .context("invalid Atlas token header")?,
        );
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(60))
            .build()
            .context("building the Atlas HTTP client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client,
        })
    }

    pub async fn summary(&self) -> Result<SummaryResponse> {
        self.get_json("/api/v1/summary").await
    }

    pub async fn formats(&self) -> Result<FormatsResponse> {
        self.get_json("/api/v1/formats").await
    }

    pub async fn list(&self, kind: &str, query: &ListQuery<'_>) -> Result<ListResponse> {
        validate_kind(kind)?;
        let params = query.params();
        let response = self
            .client
            .get(self.url(&format!("/api/v1/{kind}")))
            .query(&params)
            .send()
            .await
            .context("requesting the Atlas collection")?;
        self.decode(response).await
    }

    /// Single-asset reads are the only responses that carry consumer `data`.
    pub async fn asset(&self, kind: &str, id: &str) -> Result<AssetResponse> {
        validate_collection(kind)?;
        validate_id(id)?;
        self.get_json(&format!("/api/v1/{kind}/{id}")).await
    }

    pub async fn trash(&self) -> Result<TrashResponse> {
        self.get_json("/api/v1/trash").await
    }

    pub async fn act(&self, kind: &str, action: &str, ids: &[String]) -> Result<ActionsResponse> {
        validate_kind(kind)?;
        for id in ids {
            validate_id(id)?;
        }
        let response = self
            .client
            .post(self.url(&format!("/api/v1/{kind}/actions")))
            .json(&serde_json::json!({"action": action, "ids": ids}))
            .send()
            .await
            .context("submitting a bulk action to Atlas")?;
        self.decode(response).await
    }

    pub async fn set_pinned(&self, kind: &str, id: &str, pinned: bool) -> Result<AssetResponse> {
        validate_collection(kind)?;
        validate_id(id)?;
        let response = self
            .client
            .put(self.url(&format!("/api/v1/{kind}/{id}/pin")))
            .json(&serde_json::json!({"pinned": pinned}))
            .send()
            .await
            .context("updating the Atlas pin")?;
        self.decode(response).await
    }

    pub async fn delete(&self, kind: &str, id: &str) -> Result<()> {
        validate_collection(kind)?;
        validate_id(id)?;
        self.empty(
            self.client
                .delete(self.url(&format!("/api/v1/{kind}/{id}")))
                .send()
                .await,
        )
        .await
    }

    pub async fn restore(&self, kind: &str, id: &str) -> Result<()> {
        validate_collection(kind)?;
        validate_id(id)?;
        self.empty(
            self.client
                .post(self.url(&format!("/api/v1/{kind}/{id}/restore")))
                .send()
                .await,
        )
        .await
    }

    /// Atlas accepts a raw file body, not multipart. Keep the user-provided
    /// filename in the header only; Atlas uses its own opaque id for paths.
    pub async fn upload(
        &self,
        kind: &str,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(StatusCode, AssetResponse)> {
        validate_kind(kind)?;
        let response = self
            .client
            .post(self.url(&format!("/api/v1/{kind}")))
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header("X-Original-Filename", encode_filename(filename))
            .body(bytes)
            .send()
            .await
            .context("uploading to Atlas")?;
        let status = response.status();
        let asset = self.decode(response).await?;
        Ok((status, asset))
    }

    pub async fn srcset(&self, id: &str) -> Result<SrcsetResponse> {
        validate_id(id)?;
        self.get_json(&format!("/api/v1/images/{id}/srcset")).await
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(self.url(path))
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        self.decode(response).await
    }

    async fn decode<T: for<'de> Deserialize<'de>>(&self, response: Response) -> Result<T> {
        if !response.status().is_success() {
            return Err(self.api_error(response).await);
        }
        response.json().await.context("decoding the Atlas response")
    }

    async fn empty(&self, response: Result<Response, reqwest::Error>) -> Result<()> {
        let response = response.context("requesting an Atlas lifecycle action")?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(self.api_error(response).await)
        }
    }

    async fn api_error(&self, response: Response) -> anyhow::Error {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")?
                    .get("message")?
                    .as_str()
                    .map(str::to_owned)
            })
            .unwrap_or(body);
        anyhow!("Atlas returned {status}: {message}")
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

fn validate_kind(kind: &str) -> Result<()> {
    if matches!(kind, "images" | "documents") {
        Ok(())
    } else {
        Err(anyhow!("unknown Atlas collection"))
    }
}

fn validate_collection(kind: &str) -> Result<()> {
    if matches!(kind, "images" | "documents" | "image" | "document") {
        Ok(())
    } else {
        Err(anyhow!("unknown Atlas collection"))
    }
}

fn validate_id(id: &str) -> Result<()> {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(anyhow!("invalid Atlas asset id"))
    }
}

/// RFC 8187-style percent-encoding for the original filename header. The
/// header is metadata only; it is never interpolated into a path.
fn encode_filename(filename: &str) -> String {
    let mut encoded = String::new();
    for byte in filename.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[derive(Debug, Clone, Default)]
pub struct ListQuery<'a> {
    pub state: Option<&'a str>,
    pub q: Option<&'a str>,
    pub sort: Option<&'a str>,
    pub order: Option<&'a str>,
    pub mime: Option<&'a str>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl ListQuery<'_> {
    fn params(&self) -> Vec<(&'static str, String)> {
        let mut params = Vec::new();
        if let Some(value) = self.state {
            params.push(("state", value.to_owned()));
        }
        if let Some(value) = self.q {
            params.push(("q", value.to_owned()));
        }
        if let Some(value) = self.sort {
            params.push(("sort", value.to_owned()));
        }
        if let Some(value) = self.order {
            params.push(("order", value.to_owned()));
        }
        if let Some(value) = self.mime {
            params.push(("mime", value.to_owned()));
        }
        if let Some(value) = self.limit {
            params.push(("limit", value.to_string()));
        }
        if let Some(value) = self.offset {
            params.push(("offset", value.to_string()));
        }
        params
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SummaryResponse {
    pub images: SummaryRow,
    pub documents: SummaryRow,
    pub total: SummaryRow,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SummaryRow {
    pub count: i64,
    pub bytes: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FormatsResponse {
    pub images: AcceptedFormats,
    pub documents: AcceptedFormats,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AcceptedFormats {
    /// One media type per accepted format: the canonical spelling.
    pub formats: Vec<String>,
    /// Every media type Atlas accepts, which is more than one for a format
    /// with alternative spellings. Absent from an older Atlas, which is why
    /// it defaults to nothing rather than failing the whole listing.
    #[serde(default)]
    pub media_types: Vec<String>,
    pub groups: Vec<String>,
    pub generates_renditions: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ListResponse {
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
    pub assets: Vec<AssetResponse>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AssetResponse {
    pub id: String,
    #[serde(default)]
    pub deduplicated: Option<bool>,
    pub created_at: i64,
    pub kind: String,
    pub state: String,
    #[serde(default)]
    pub deleted_at: Option<i64>,
    #[serde(default)]
    pub purged_at: Option<i64>,
    pub pinned: bool,
    pub original: OriginalResponse,
    pub renditions: Vec<RenditionRow>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OriginalResponse {
    pub url: String,
    #[serde(default)]
    pub filename: Option<String>,
    pub content_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RenditionRow {
    pub max_width: u32,
    pub width: u32,
    pub height: u32,
    pub url: String,
    pub formats: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TrashResponse {
    pub assets: Vec<TrashEntry>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TrashEntry {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub created_at: i64,
    pub deleted_at: Option<i64>,
    pub purged_at: Option<i64>,
    pub pinned: bool,
    pub bytes: u64,
    pub content_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ActionsResponse {
    pub results: Vec<ActionResult>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ActionResult {
    pub id: String,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SrcsetResponse {
    pub src: String,
    pub srcset: String,
}

#[cfg(test)]
mod tests {
    use super::{encode_filename, validate_collection, validate_id, validate_kind};

    #[test]
    fn filename_is_encoded_as_header_metadata_not_a_path() {
        assert_eq!(
            encode_filename("Unsere Familie.png"),
            "Unsere%20Familie.png"
        );
        assert_eq!(encode_filename("100% pure.pdf"), "100%25%20pure.pdf");
        assert_eq!(
            encode_filename("../../etc/passwd"),
            "..%2F..%2Fetc%2Fpasswd"
        );
    }

    #[test]
    fn routes_only_accept_known_collections_and_opaque_base36_ids() {
        assert!(validate_kind("images").is_ok());
        assert!(validate_kind("documents").is_ok());
        assert!(
            validate_kind("image").is_err(),
            "collection lists use slugs"
        );
        assert!(
            validate_collection("image").is_ok(),
            "response kind is accepted on a per-asset route"
        );
        assert!(validate_collection("documents").is_ok());
        assert!(validate_collection("../data").is_err());
        assert!(validate_id("uix1zbhwgea1a11rtz").is_ok());
        assert!(validate_id("../../etc/passwd").is_err());
        assert!(validate_id("UpperCase").is_err());
    }
}
