use serde::{Deserialize, Serialize};
use url::Url;

/// Custom Azure Blob Storage configuration for an `az://` channel container.
#[derive(Debug, Clone, PartialEq, Serialize, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AzureOptions {
    /// Name of the storage account backing the container.
    pub account: String,
    /// Optional endpoint URL override (sovereign clouds / custom endpoints).
    /// When absent the default `https://{account}.blob.core.windows.net`
    /// endpoint is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_url: Option<Url>,
}
