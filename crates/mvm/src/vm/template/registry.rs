use anyhow::{Context, Result, anyhow, bail};

use opendal::Operator;
use opendal::services::S3;

pub struct TemplateRegistry {
    op: opendal::BlockingOperator,
    prefix: String,
}

impl TemplateRegistry {
    pub fn from_env() -> Result<Option<Self>> {
        let endpoint = match std::env::var("MVM_TEMPLATE_REGISTRY_ENDPOINT") {
            Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => return Ok(None),
        };
        let insecure = std::env::var("MVM_TEMPLATE_REGISTRY_INSECURE")
            .ok()
            .map(|v| v.to_ascii_lowercase())
            .map(|v| v == "1" || v == "true" || v == "yes")
            .unwrap_or(false);
        if endpoint.starts_with("http://") && !insecure {
            bail!(
                "Template registry endpoint is http:// but MVM_TEMPLATE_REGISTRY_INSECURE is not true"
            );
        }
        let bucket = std::env::var("MVM_TEMPLATE_REGISTRY_BUCKET")
            .map_err(anyhow::Error::new)
            .context("MVM_TEMPLATE_REGISTRY_BUCKET must be set when registry endpoint is set")?;

        let access_key = std::env::var("MVM_TEMPLATE_REGISTRY_ACCESS_KEY_ID")
            .map_err(anyhow::Error::new)
            .context("MVM_TEMPLATE_REGISTRY_ACCESS_KEY_ID must be set")?;
        let secret_key = std::env::var("MVM_TEMPLATE_REGISTRY_SECRET_ACCESS_KEY")
            .map_err(anyhow::Error::new)
            .context("MVM_TEMPLATE_REGISTRY_SECRET_ACCESS_KEY must be set")?;

        let prefix = std::env::var("MVM_TEMPLATE_REGISTRY_PREFIX")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "mvm".to_string());
        let prefix = prefix.trim_matches('/').to_string();

        let region = std::env::var("MVM_TEMPLATE_REGISTRY_REGION")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "us-east-1".to_string());

        // NOTE: OpenDAL's service builders use a consuming builder pattern (method calls take and
        // return `Self`), so keep this as a chained expression.
        //
        // MinIO commonly uses path-style requests; OpenDAL's S3 service defaults are compatible
        // as long as the endpoint points at your MinIO/S3-compatible gateway.
        let builder = S3::default()
            .endpoint(&endpoint)
            .bucket(bucket.trim())
            .region(region.trim())
            .access_key_id(access_key.trim())
            .secret_access_key(secret_key.trim());

        let op = Operator::new(builder)?.finish().blocking();

        Ok(Some(Self { op, prefix }))
    }

    pub fn key_current(&self, template_id: &str) -> String {
        format!(
            "{}/templates/{}/current",
            self.prefix,
            template_id.trim_matches('/')
        )
    }

    pub fn key_revision_base(&self, template_id: &str, revision: &str) -> String {
        format!(
            "{}/templates/{}/revisions/{}",
            self.prefix,
            template_id.trim_matches('/'),
            revision
        )
    }

    pub fn key_revision_file(&self, template_id: &str, revision: &str, file: &str) -> String {
        format!("{}/{}", self.key_revision_base(template_id, revision), file)
    }

    pub fn put_bytes(&self, key: &str, data: Vec<u8>) -> Result<()> {
        self.op
            .write(key, data)
            .map_err(anyhow::Error::new)
            .with_context(|| format!("Failed to write object {}", key))?;
        Ok(())
    }

    pub fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let data = self
            .op
            .read(key)
            .map_err(anyhow::Error::new)
            .with_context(|| format!("Failed to read object {}", key))?;
        Ok(data.to_vec())
    }

    pub fn put_text(&self, key: &str, text: &str) -> Result<()> {
        self.put_bytes(key, text.as_bytes().to_vec())
    }

    pub fn get_text(&self, key: &str) -> Result<String> {
        let bytes = self.get_bytes(key)?;
        String::from_utf8(bytes).map_err(|e| anyhow!("invalid utf-8 in object {}: {}", key, e))
    }

    pub fn require_configured(&self) -> Result<()> {
        if self.prefix.is_empty() {
            bail!("Template registry prefix is empty");
        }
        Ok(())
    }
}
