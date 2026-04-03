use crate::config::{AppConfig, S3Config, StorageConfig};
use anyhow::Result;
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Region},
    primitives::ByteStream,
};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use tokio::fs;

#[derive(Clone)]
pub struct Storage {
    backend: StorageBackend,
}

#[derive(Clone)]
enum StorageBackend {
    Local { root: PathBuf },
    S3 { bucket: String, client: Client },
}

impl Storage {
    pub async fn new(config: &AppConfig) -> Result<Self> {
        let backend = match &config.storage {
            StorageConfig::Local { root } => {
                fs::create_dir_all(root).await?;
                StorageBackend::Local { root: root.clone() }
            }
            StorageConfig::S3(s3) => StorageBackend::S3 {
                bucket: s3.bucket.clone(),
                client: build_s3_client(s3),
            },
        };

        Ok(Self { backend })
    }

    pub async fn upload_bytes(&self, key: &str, bytes: Bytes, content_type: &str) -> Result<()> {
        match &self.backend {
            StorageBackend::Local { root } => {
                let path = path_for_key(root, key);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).await?;
                }

                fs::write(path, bytes).await?;
                let _ = content_type;
                Ok(())
            }
            StorageBackend::S3 { bucket, client } => {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .content_type(content_type)
                    .body(ByteStream::from(bytes.to_vec()))
                    .send()
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn get_bytes(&self, key: &str) -> Result<(Bytes, Option<String>)> {
        match &self.backend {
            StorageBackend::Local { root } => {
                let path = path_for_key(root, key);
                let bytes = fs::read(path).await?;
                Ok((Bytes::from(bytes), None))
            }
            StorageBackend::S3 { bucket, client } => {
                let response = client.get_object().bucket(bucket).key(key).send().await?;
                let content_type = response.content_type().map(ToOwned::to_owned);
                let bytes = response.body.collect().await?.into_bytes();
                Ok((bytes, content_type))
            }
        }
    }
}

fn build_s3_client(config: &S3Config) -> Client {
    let mut s3_config = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(config.region.clone()))
        .credentials_provider(Credentials::new(
            &config.access_key_id,
            &config.secret_access_key,
            None,
            None,
            "static-config",
        ));

    if let Some(endpoint) = &config.endpoint {
        s3_config = s3_config.endpoint_url(endpoint);
    }

    if config.force_path_style {
        s3_config = s3_config.force_path_style(true);
    }

    Client::from_conf(s3_config.build())
}

fn path_for_key(root: &Path, key: &str) -> PathBuf {
    key.split('/')
        .filter(|segment| !segment.is_empty())
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}
