use crate::remotefs::{LocalDirRemoteFs, RemoteFile, RemoteFs};
use crate::CubeError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use log::{debug, info};
use regex::{NoExpand, Regex};
use s3::creds::Credentials;
use s3::Bucket;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::RwLock;
use std::io::Write;
use std::time::SystemTime;

#[derive(Debug)]
pub struct S3RemoteFs {
    dir: RwLock<PathBuf>,
    bucket: Bucket,
    sub_path: Option<String>,
}

impl S3RemoteFs {
    pub fn new(
        dir: PathBuf,
        region: String,
        bucket_name: String,
        sub_path: Option<String>,
    ) -> Result<Arc<Self>, CubeError> {
        let credentials = Credentials::default()?;
        let bucket = Bucket::new(&bucket_name, region.parse()?, credentials)?;
        Ok(Arc::new(Self {
            dir: RwLock::new(dir),
            bucket,
            sub_path,
        }))
    }
}

#[async_trait]
impl RemoteFs for S3RemoteFs {
    async fn upload_file(&self, remote_path: &str) -> Result<(), CubeError> {
        let time = SystemTime::now();
        debug!("Uploading {}", remote_path);
        let status_code = self
            .bucket
            .put_object_stream(
                self.dir.read().await.as_path().join(remote_path),
                self.s3_path(remote_path),
            )
            .await?;
        info!("Uploaded {} ({:?})", remote_path, time.elapsed()?);
        if status_code != 200 {
            return Err(CubeError::user(format!(
                "S3 upload returned non OK status: {}",
                status_code
            )));
        }
        Ok(())
    }

    async fn download_file(&self, remote_path: &str) -> Result<String, CubeError> {
        let local = self.dir.write().await.as_path().join(remote_path);
        let path = local.to_str().unwrap().to_owned();
        fs::create_dir_all(local.parent().unwrap()).await?;
        if !local.exists() {
            let time = SystemTime::now();
            debug!("Downloading {}", remote_path);
            let mut output_file = std::fs::File::create(path.as_str())?;
            let status_code = self
                .bucket
                .get_object_stream(self.s3_path(remote_path), &mut output_file)
                .await?;
            // TODO async
            output_file.flush()?;
            info!("Downloaded {} ({:?})", remote_path, time.elapsed()?);
            if status_code != 200 {
                return Err(CubeError::user(format!(
                    "S3 download returned non OK status: {}",
                    status_code
                )));
            }
        }
        Ok(path)
    }

    async fn delete_file(&self, remote_path: &str) -> Result<(), CubeError> {
        let time = SystemTime::now();
        debug!("Deleting {}", remote_path);
        let (_, status_code) = self.bucket.delete_object(self.s3_path(remote_path)).await?;
        info!("Deleting {} ({:?})", remote_path, time.elapsed()?);
        if status_code != 204 {
            return Err(CubeError::user(format!(
                "S3 delete returned non OK status: {}",
                status_code
            )));
        }

        let dir = self.dir.write().await;
        let local = dir.as_path().join(remote_path);
        if fs::metadata(local.clone()).await.is_ok() {
            fs::remove_file(local.clone()).await?;
            LocalDirRemoteFs::remove_empty_paths(dir.as_path().to_path_buf(), local.clone())
                .await?;
        }

        Ok(())
    }

    async fn list(&self, remote_prefix: &str) -> Result<Vec<String>, CubeError> {
        Ok(self
            .list_with_metadata(remote_prefix)
            .await?
            .into_iter()
            .map(|f| f.remote_path)
            .collect::<Vec<_>>())
    }

    async fn list_with_metadata(&self, remote_prefix: &str) -> Result<Vec<RemoteFile>, CubeError> {
        let list = self.bucket.list(self.s3_path(remote_prefix), None).await?;
        let leading_slash = Regex::new(format!("^{}", self.s3_path("")).as_str()).unwrap();
        let result = list
            .iter()
            .flat_map(|res| {
                res.contents
                    .iter()
                    .map(|o| -> Result<RemoteFile, CubeError> {
                        Ok(RemoteFile {
                            remote_path: leading_slash.replace(&o.key, NoExpand("")).to_string(),
                            updated: DateTime::parse_from_rfc3339(&o.last_modified)?
                                .with_timezone(&Utc),
                        })
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(result)
    }

    async fn local_path(&self) -> String {
        self.dir.read().await.to_str().unwrap().to_owned()
    }

    async fn local_file(&self, remote_path: &str) -> Result<String, CubeError> {
        let buf = self.dir.read().await.join(remote_path);
        fs::create_dir_all(buf.parent().unwrap()).await?;
        Ok(buf.to_str().unwrap().to_string())
    }
}

impl S3RemoteFs {
    fn s3_path(&self, remote_path: &str) -> String {
        format!(
            "{}/{}",
            self.sub_path
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "".to_string()),
            remote_path
        )
    }
}
