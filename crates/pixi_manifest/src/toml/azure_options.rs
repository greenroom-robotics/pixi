use crate::AzureOptions;

use pixi_toml::TomlFromStr;
use toml_span::{DeserError, Value, de_helpers::TableHelper};

impl<'de> toml_span::Deserialize<'de> for AzureOptions {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;

        let account = th.required("account")?;
        let endpoint_url = th
            .optional::<TomlFromStr<_>>("endpoint-url")
            .map(TomlFromStr::into_inner);
        th.finalize(None)?;

        Ok(Self {
            account,
            endpoint_url,
        })
    }
}
