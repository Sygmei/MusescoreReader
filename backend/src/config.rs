use anyhow::{Result, anyhow};
use std::{env, path::PathBuf};

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub bind_address: String,
    pub admin_password: String,
    pub app_base_url: String,
    pub database_path: PathBuf,
    pub storage: StorageConfig,
    pub musescore_bin: Option<String>,
    pub soundfont_dir: Option<PathBuf>,
    pub sfizz_bin: Option<String>,
}

#[derive(Clone, Debug)]
pub enum StorageConfig {
    Local { root: PathBuf },
    S3(S3Config),
}

#[derive(Clone, Debug)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub endpoint: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub force_path_style: bool,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let bind_address = env::var("BIND_ADDRESS").unwrap_or_else(|_| "127.0.0.1:3000".to_owned());
        let admin_password =
            env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "musescore-admin".to_owned());
        let app_base_url =
            env::var("APP_BASE_URL").unwrap_or_else(|_| "http://localhost:5173".to_owned());
        let database_path = PathBuf::from(
            env::var("DATABASE_PATH").unwrap_or_else(|_| "./data/musescore-reader.db".to_owned()),
        );
        let local_storage_path = PathBuf::from(
            env::var("LOCAL_STORAGE_PATH").unwrap_or_else(|_| "./data/storage".to_owned()),
        );

        let s3_bucket = env::var("S3_BUCKET")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let s3_access_key_id = env::var("S3_ACCESS_KEY_ID")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let s3_secret_access_key = env::var("S3_SECRET_ACCESS_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let storage = match (s3_bucket, s3_access_key_id, s3_secret_access_key) {
            (Some(bucket), Some(access_key_id), Some(secret_access_key)) => {
                StorageConfig::S3(S3Config {
                    bucket,
                    region: env::var("S3_REGION").unwrap_or_else(|_| "eu-west-3".to_owned()),
                    endpoint: env::var("S3_ENDPOINT")
                        .ok()
                        .filter(|value| !value.trim().is_empty()),
                    access_key_id,
                    secret_access_key,
                    force_path_style: env::var("S3_FORCE_PATH_STYLE")
                        .map(|value| {
                            matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES")
                        })
                        .unwrap_or(false),
                })
            }
            (None, None, None) => StorageConfig::Local {
                root: local_storage_path,
            },
            _ => {
                return Err(anyhow!(
                    "To enable S3 storage, set S3_BUCKET, S3_ACCESS_KEY_ID, and S3_SECRET_ACCESS_KEY. Otherwise leave them unset to use local storage."
                ));
            }
        };

        let musescore_bin = env::var("MUSESCORE_BIN")
            .ok()
            .filter(|value| !value.trim().is_empty());

        let soundfont_dir = env::var("SOUNDFONT_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);

        let sfizz_bin = env::var("SFIZZ_BIN")
            .ok()
            .filter(|value| !value.trim().is_empty());

        Ok(Self {
            bind_address,
            admin_password,
            app_base_url,
            database_path,
            storage,
            musescore_bin,
            soundfont_dir,
            sfizz_bin,
        })
    }

    pub fn public_url_for(&self, access_key: &str) -> String {
        format!(
            "{}/listen/{}",
            self.app_base_url.trim_end_matches('/'),
            access_key
        )
    }
}
