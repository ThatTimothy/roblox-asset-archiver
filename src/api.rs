use std::{
    collections::HashMap,
    fmt::Display,
    time::{Duration, Instant},
};

use log::{trace, warn};
use reqwest::{IntoUrl, Method, Request, Response, StatusCode, Url, header::HeaderMap};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{sync::Mutex, time::sleep};

const API_BASE: &'static str = "https://apis.roblox.com";
const API_HOST: &'static str = "apis.roblox.com";

#[derive(Deserialize, Debug)]
pub struct APICodeMessageError {
    pub code: i64,
    pub message: String,
}

/// Represents the two types of possible API errors
/// Docs: https://create.roblox.com/docs/cloud/reference/errors
#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum APIError {
    Gateway { errors: Vec<APICodeMessageError> },
    OpenCloud(APICodeMessageError),
}

impl Display for APIError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            APIError::Gateway { errors } => {
                for (i, error) in errors.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{} (code {})", error.message, error.code)?;
                }
                Ok(())
            }
            APIError::OpenCloud(error) => write!(f, "{} (code {})", error.message, error.code),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum RequestError {
    /// Server responded with standard error format
    #[error("{error} ({status} {method} {url})")]
    Api {
        method: Method,
        url: String,
        status: StatusCode,
        headers: HeaderMap,
        error: APIError,
    },

    /// Failed to parse a response or error
    #[error("Failed to parse response {method} {url}, got body:\n{body}")]
    Parse {
        method: Method,
        url: String,
        body: String,
        #[source]
        source: serde_json::Error,
    },

    /// Generic request error
    #[error("Failed to request")]
    Request {
        method: Method,
        url: String,
        #[source]
        source: reqwest::Error,
    },
}
pub type Result<T> = std::result::Result<T, RequestError>;

struct RatelimitBucket {
    pub remaining: u64,
    pub reset: Instant,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IntrospectRequest {
    api_key: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IntrospectScope {
    pub name: String,
    pub operations: Vec<String>,
    #[serde(default)]
    pub group_ids: Vec<String>,
    #[serde(default)]
    pub user_ids: Vec<String>,
}

impl Display for IntrospectScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scoped_on = self
            .group_ids
            .iter()
            .map(|group_id| format!("group {group_id}"))
            .chain(
                self.user_ids
                    .iter()
                    .map(|user_id| format!("user {user_id}")),
            )
            .collect::<Vec<_>>();
        write!(
            f,
            "{}:{}({})",
            self.name,
            self.operations.join("|"),
            if scoped_on.is_empty() {
                "all".to_owned()
            } else {
                scoped_on.join(", ")
            },
        )
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IntrospectResponse {
    pub name: String,
    pub authorized_user_id: u64,
    pub scopes: Vec<IntrospectScope>,
    pub enabled: bool,
    pub expired: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UniverseContainingPlaceResponse {
    pub universe_id: Option<u64>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Universe {
    /// Full path containing universe_id: `universes/{universe_id}``
    pub path: String,
    pub display_name: String,
    pub description: String,
    pub create_time: String,
    pub update_time: String,
    /// Full path containing user_id: `users/{user_id}`
    pub user: Option<String>,
    /// Full path containing group_id: `groups/{group_id}`
    pub group: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Place {
    /// Full path containing universe_id and place_id: `universes/{universe_id}/places/{place_id}`
    pub path: String,
    pub create_time: String,
    pub update_time: String,
    pub display_name: String,
    pub description: String,
    pub server_size: u64,
}

impl Place {
    /// Parses universe_id from [Place::path]
    pub fn universe_id(&self) -> u64 {
        self.path
            .split("/")
            .nth(1)
            .expect("Malformed place path")
            .parse()
            .expect("Malformed place path")
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlaceVersion {
    pub version: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub contributors: Vec<u64>,
    pub save_type: i64,
    pub is_published: bool,
    pub publish_status: i64,
    pub has_notes: bool,
    pub created_time: String,
    pub created_by: Option<u64>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlaceVersionsResponse {
    pub next_cursor: Option<String>,
    pub has_more: bool,
    pub place_versions: Vec<PlaceVersion>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AssetDeliveryMetadata {
    pub metadata_type: i64,
    pub value: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AssetDeliveryResponse {
    pub location: String,
    /// If exists, assetMetadataType == 1 is the uncompressed length in bytes
    pub asset_metadatas: Option<Vec<AssetDeliveryMetadata>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AssetVersionCreator {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub group_id: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AssetVersionCreationContext {
    pub asset_privacy: Option<String>,
    pub creator: AssetVersionCreator,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AssetVersionModerationResult {
    pub moderation_state: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AssetVersion {
    pub creation_context: AssetVersionCreationContext,
    pub path: String,
    pub moderation_result: AssetVersionModerationResult,
    pub create_time: String,
    pub published: Option<bool>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AssetVersionsResponse {
    pub asset_versions: Vec<AssetVersion>,
    pub next_page_token: Option<String>,
}

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    ratelimits: Mutex<HashMap<String, RatelimitBucket>>,
}

impl Client {
    pub fn new(api_key: String) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .unwrap();
        Self {
            ratelimits: Mutex::new(HashMap::new()),
            http,
            api_key,
        }
    }

    /// Sends a request, parses error out and returns raw response in success case
    /// Errors docs: https://create.roblox.com/docs/cloud/reference/errors
    /// Rate limit docs: https://create.roblox.com/docs/cloud/reference/rate-limits
    async fn send_request(&self, ratelimit_key: String, mut request: Request) -> Result<Response> {
        let method = request.method().clone();
        let url = request.url().to_string();
        // By default, query params are stripped (this is mainly for non-api URLs that expose secrets in query)
        let mut redacted_url = {
            let mut redacted = request.url().clone();
            redacted.set_query(None);
            format!("{} (redacted query) ", redacted.to_string())
        };

        // API specific handling
        if request.url().host_str() == Some(API_HOST) {
            request
                .headers_mut()
                .insert("X-API-KEY", self.api_key.parse().unwrap());
            // API does not expose secret in query
            redacted_url = url.clone();
        }

        // Keep retrying on ratelimits
        loop {
            // Before requesting, check for existing ratelimit
            let retry_after: Option<Duration> = {
                let mut ratelimits = self.ratelimits.lock().await;
                let bucket = ratelimits
                    .entry(ratelimit_key.clone())
                    .or_insert(RatelimitBucket {
                        remaining: 1,
                        reset: Instant::now(),
                    });
                let now = Instant::now();
                // If no requests remaining and before reset, need to wait until reset
                if bucket.remaining <= 0 && bucket.reset > now {
                    Some(bucket.reset - now)
                } else {
                    None
                }
            };

            // If need to retry after, wait then reset remaining to 1 (we will get the right value from header)
            if let Some(retry_after) = retry_after {
                warn!(
                    "Out of requests on {redacted_url}, retrying in {:.2}s to avoid ratelimit",
                    retry_after.as_secs_f64()
                );
                sleep(retry_after).await;

                let mut ratelimits = self.ratelimits.lock().await;
                ratelimits.get_mut(&ratelimit_key).unwrap().remaining = 1;
                trace!("Finished waiting, retrying {ratelimit_key} with remaining=1");
            }

            {
                let mut ratelimits = self.ratelimits.lock().await;
                let bucket = ratelimits.get_mut(&ratelimit_key).unwrap();
                if bucket.remaining > 0 {
                    bucket.remaining -= 1;
                }
            }

            trace!("Requesting {method} {redacted_url}");
            let req_err = |source| RequestError::Request {
                method: method.clone(),
                url: redacted_url.clone(),
                source,
            };
            let response = self
                .http
                .execute(request.try_clone().unwrap())
                .await
                .map_err(req_err)?;
            let status = response.status();

            trace!("{status} on {method} {redacted_url}");

            // Ratelimits automatically retried based on headers
            let headers = response.headers();
            let remaining_header: Option<u64> = headers
                .get("x-ratelimit-remaining")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            let reset_header: Option<u64> = headers
                .get("x-ratelimit-reset")
                .and_then(|value| value.to_str().ok())
                .or_else(|| {
                    headers
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok())
                })
                .and_then(|value| value.parse().ok());

            if let Some(remaining) = remaining_header
                && let Some(reset) = reset_header
            {
                let mut ratelimits = self.ratelimits.lock().await;
                let bucket = ratelimits.get_mut(&ratelimit_key).unwrap();
                // If a true reset, remaining will go from 0 back up
                if bucket.remaining == 0 && remaining > 0 {
                    trace!(
                        "Detected bucket reset, updating {ratelimit_key} remaining to {remaining}"
                    );
                    bucket.remaining = remaining;
                }

                // Always update reset
                bucket.reset = Instant::now() + Duration::from_secs(reset);
            }

            // If we hit this status, we messed up tracking - sleep and retry
            if status == StatusCode::TOO_MANY_REQUESTS {
                {
                    let mut ratelimits = self.ratelimits.lock().await;
                    ratelimits.get_mut(&ratelimit_key).unwrap().remaining = 0;
                }
                let retry_after = reset_header.unwrap_or(5);
                warn!(
                    "Ratelimited on {redacted_url}, retrying in {:.2}s",
                    retry_after
                );
                sleep(Duration::from_secs(retry_after)).await;
                trace!("Finished waiting, retrying {ratelimit_key}");
                continue;
            }

            if !status.is_success() {
                let headers = response.headers().clone();
                let text = response.text().await.map_err(req_err)?;
                return Err(RequestError::Api {
                    method: method.clone(),
                    url: url.clone(),
                    status,
                    headers: headers,
                    error: serde_json::from_str(&text).map_err(|source| RequestError::Parse {
                        method: method.clone(),
                        url: redacted_url.clone(),
                        body: text.clone(),
                        source,
                    })?,
                });
            }

            return Ok(response);
        }
    }

    /// Sends a request and handles deserialization
    async fn send_request_and_deserialize<Res: DeserializeOwned>(
        &self,
        ratelimit_key: String,
        request: Request,
    ) -> Result<Res> {
        let method = request.method().clone();
        let url = request.url().to_string();
        let response = self.send_request(ratelimit_key, request).await?;
        let text = response
            .text()
            .await
            .map_err(|source| RequestError::Request {
                method: method.clone(),
                url: url.clone(),
                source,
            })?;
        Ok(
            serde_json::from_str(&text).map_err(|source| RequestError::Parse {
                method: method.clone(),
                url: url.clone(),
                body: text.clone(),
                source,
            })?,
        )
    }

    /// Gets a specific Url and attempts to deserialize response
    pub async fn get<U: IntoUrl, Res: DeserializeOwned>(
        &self,
        ratelimit_key: String,
        url: U,
    ) -> Result<Res> {
        self.send_request_and_deserialize(
            ratelimit_key,
            self.http.get(url).build().expect("Failed to build request"),
        )
        .await
    }

    /// Gets a specific Url, serializing body, and attempts to deserialize response
    pub async fn post<U: IntoUrl, Req: Serialize, Res: DeserializeOwned>(
        &self,
        ratelimit_key: String,
        url: U,
        body: Req,
    ) -> Result<Res> {
        self.send_request_and_deserialize(
            ratelimit_key,
            self.http
                .post(url)
                .json(&body)
                .build()
                .expect("Failed to build request"),
        )
        .await
    }

    // Gets a specific Url, handling errors, returning the raw response if successful
    pub async fn raw_get<U: IntoUrl>(&self, ratelimit_key: String, url: U) -> Result<Response> {
        self.send_request(
            ratelimit_key,
            self.http.get(url).build().expect("Failed to build request"),
        )
        .await
    }

    /// Returns introspection of an API key - can be used to validate if an API key is enabled and not expired
    /// Docs: https://create.roblox.com/docs/cloud/auth/api-keys#introspect-api-keys
    pub async fn introspect(&self) -> Result<IntrospectResponse> {
        self.post(
            "introspect".to_owned(),
            format!("{API_BASE}/api-keys/v1/introspect"),
            IntrospectRequest {
                api_key: self.api_key.clone(),
            },
        )
        .await
    }

    /// Gets the universe id for a place id
    /// Requires no authentication
    /// Undocumented (see https://devforum.roblox.com/t/httpsapisrobloxcomuniversesv1placesplaceiduniverse-endpoint-is-not-documented/4915512)
    pub async fn universe_containing_place(
        &self,
        place_id: u64,
    ) -> Result<UniverseContainingPlaceResponse> {
        self.get(
            "universe_containing_place".to_owned(),
            format!("{API_BASE}/universes/v1/places/{place_id}/universe"),
        )
        .await
    }

    /// Gets details for a universe
    /// Requires no authentication
    /// Docs: https://create.roblox.com/docs/en-us/cloud/reference/features/universes#Cloud_GetUniverse
    pub async fn universe(&self, universe_id: u64) -> Result<Universe> {
        self.get(
            "universe".to_owned(),
            format!("{API_BASE}/cloud/v2/universes/{universe_id}"),
        )
        .await
    }

    /// Gets details for a place
    /// Requires no authentication
    /// Docs: https://create.roblox.com/docs/en-us/cloud/reference/features/places#Cloud_GetPlace
    pub async fn place(&self, universe_id: u64, place_id: u64) -> Result<Place> {
        self.get(
            "place".to_owned(),
            format!("{API_BASE}/cloud/v2/universes/{universe_id}/places/{place_id}"),
        )
        .await
    }

    /// Gets a page of versions of an place
    async fn place_versions_page(
        &self,
        place_id: u64,
        next_cursor: Option<String>,
    ) -> Result<PlaceVersionsResponse> {
        let mut url = Url::parse(&format!(
            "{API_BASE}/place-version-history-api/v1/{place_id}/history"
        ))
        .unwrap();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("pageSize", "101");
            if let Some(next_cursor) = next_cursor {
                query.append_pair("cursor", &next_cursor);
            }
        }
        self.get("place_versions_page".to_owned(), url).await
    }

    /// Gets all versions of a place, with notes and contributor information
    /// Requires universe.place:read ON EVERYTHING (see https://devforum.roblox.com/t/getplaceversionhistory-endpoint-should-not-require-scope-to-all-resources/4976529 for feature request)
    /// Docs: https://create.roblox.com/docs/en-us/cloud/reference/domains/apis#PlaceVersion_GetPlaceVersionHistory
    pub async fn place_versions(&self, place_id: u64) -> Result<Vec<PlaceVersion>> {
        let mut results = Vec::new();

        let mut next_cursor = None;
        loop {
            let result = self.place_versions_page(place_id, next_cursor).await?;
            trace!(
                "Got page with {} results, nextCursor: {}",
                result.place_versions.len(),
                result.next_cursor.clone().unwrap_or("None".to_owned())
            );

            for place_version in result.place_versions {
                results.push(place_version);
            }

            next_cursor = result.next_cursor.clone();

            if next_cursor == None || next_cursor.clone().unwrap().is_empty() {
                break;
            }
        }

        Ok(results)
    }

    /// Gets an asset delivery URL of an asset, use Client::download_o_fil
    /// Requires legacy-asset:manage
    /// Docs: https://create.roblox.com/docs/en-us/cloud/reference/domains/apis#get_asset_delivery_api_v1_assetId__assetId__version__versionNumber_
    pub async fn asset_version_delivery(
        &self,
        asset_id: u64,
        version: u64,
    ) -> Result<AssetDeliveryResponse> {
        self.get(
            "asset_version_delivery".to_owned(),
            format!("{API_BASE}/asset-delivery-api/v1/assetId/{asset_id}/version/{version}"),
        )
        .await
    }

    /// Gets a page of versions of an asset
    async fn asset_versions_page(
        &self,
        asset_id: u64,
        next_page_token: Option<String>,
    ) -> Result<AssetVersionsResponse> {
        let mut url =
            Url::parse(&format!("{API_BASE}/assets/v1/assets/{asset_id}/versions")).unwrap();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("maxPageSize", "50");
            if let Some(next_page_token) = next_page_token {
                query.append_pair("pageToken", &next_page_token);
            }
        }
        self.get("asset_versions_page".to_owned(), url).await
    }

    /// Gets all versions of an asset
    /// Requires asset:read
    /// Docs: https://create.roblox.com/docs/cloud/reference/features/assets#listAssetVersions
    /// Note: inconsistent with docs, see https://devforum.roblox.com/t/listassetversions-example-is-completely-wrong/4976456
    pub async fn asset_versions(&self, asset_id: u64) -> Result<Vec<AssetVersion>> {
        let mut results = Vec::new();

        let mut next_page_token = None;
        loop {
            let result = self.asset_versions_page(asset_id, next_page_token).await?;
            trace!(
                "Got page with {} results, nextPageToken: {}",
                result.asset_versions.len(),
                result.next_page_token.clone().unwrap_or("None".to_owned())
            );

            for asset_version in result.asset_versions {
                results.push(asset_version);
            }

            next_page_token = result.next_page_token.clone();

            if next_page_token == None {
                break;
            }
        }

        Ok(results)
    }
}
