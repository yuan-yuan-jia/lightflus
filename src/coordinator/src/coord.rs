use std::collections::BTreeMap;

use common::err::CommonException;
use common::err::ErrorKind;
use common::net::cluster;

use common::types::HashedResourceId;
use common::utils;
use prost::Message;
use proto::common::Dataflow;
use proto::common::DataflowStatus;
use proto::common::ResourceId;

pub(crate) trait DataflowStorage {
    fn save(&mut self, dataflow: Dataflow) -> Result<(), CommonException>;
    fn get(&self, job_id: &ResourceId) -> Option<Dataflow>;
    fn may_exists(&self, job_id: &ResourceId) -> bool;
    fn delete(&mut self, job_id: &ResourceId) -> Result<(), CommonException>;
}

#[derive(Clone, Debug)]
pub struct PersistDataflowStorage {
    db: sled::Db,
}

impl DataflowStorage for PersistDataflowStorage {
    fn save(&mut self, dataflow: Dataflow) -> Result<(), CommonException> {
        self.db
            .insert(
                dataflow
                    .job_id
                    .as_ref()
                    .map(|key| key.encode_to_vec())
                    .unwrap_or_default(),
                dataflow.encode_to_vec(),
            )
            .map(|_| {})
            .map_err(|err| CommonException {
                kind: ErrorKind::SaveDataflowFailed,
                message: err.to_string(),
            })
    }

    fn get(&self, job_id: &ResourceId) -> Option<Dataflow> {
        match self
            .db
            .get(&job_id.encode_to_vec())
            .map(|data| data.and_then(|buf| utils::from_pb_slice(&buf).ok()))
            .map_err(|err| CommonException {
                kind: ErrorKind::GetDataflowFailed,
                message: err.to_string(),
            }) {
            Ok(result) => result,
            Err(err) => {
                tracing::error!("get dataflow {:?} failed because: {:?}", job_id, err);
                None
            }
        }
    }

    fn may_exists(&self, job_id: &ResourceId) -> bool {
        self.db
            .contains_key(job_id.encode_to_vec())
            .unwrap_or(false)
    }

    fn delete(&mut self, job_id: &ResourceId) -> Result<(), CommonException> {
        self.db
            .remove(job_id.encode_to_vec())
            .map(|_| {})
            .map_err(|err| CommonException {
                kind: ErrorKind::DeleteDataflowFailed,
                message: err.to_string(),
            })
    }
}

#[derive(Clone, Debug, Default)]
pub struct MemDataflowStorage {
    cache: BTreeMap<HashedResourceId, Dataflow>,
}

impl DataflowStorage for MemDataflowStorage {
    fn save(&mut self, dataflow: Dataflow) -> Result<(), CommonException> {
        self.cache.insert(
            HashedResourceId::from(dataflow.job_id.as_ref().unwrap()),
            dataflow.clone(),
        );
        Ok(())
    }

    fn get(&self, job_id: &ResourceId) -> Option<Dataflow> {
        self.cache
            .get(&HashedResourceId::from(job_id))
            .map(|dataflow| dataflow.clone())
    }

    fn may_exists(&self, job_id: &ResourceId) -> bool {
        self.cache.contains_key(&job_id.into())
    }

    fn delete(&mut self, job_id: &ResourceId) -> Result<(), CommonException> {
        self.cache.remove(&job_id.into());
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum DataflowStorageImpl {
    Persist(PersistDataflowStorage),
    Memory(MemDataflowStorage),
}

impl DataflowStorageImpl {
    fn save(&mut self, dataflow: Dataflow) -> Result<(), CommonException> {
        match self {
            Self::Persist(storage) => storage.save(dataflow),
            Self::Memory(storage) => storage.save(dataflow),
        }
    }

    fn get(&self, job_id: &ResourceId) -> Option<Dataflow> {
        match self {
            Self::Persist(storage) => storage.get(job_id),
            Self::Memory(storage) => storage.get(job_id),
        }
    }

    fn may_exists(&self, job_id: &ResourceId) -> bool {
        match self {
            Self::Persist(storage) => storage.may_exists(job_id),
            Self::Memory(storage) => storage.may_exists(job_id),
        }
    }

    fn delete(&mut self, job_id: &ResourceId) -> Result<(), CommonException> {
        match self {
            DataflowStorageImpl::Persist(storage) => storage.delete(job_id),
            DataflowStorageImpl::Memory(storage) => storage.delete(job_id),
        }
    }
}

#[derive(Clone)]
pub struct Coordinator {
    dataflow_storage: DataflowStorageImpl,
    cluster: cluster::Cluster,
}

impl Coordinator {
    pub fn new(
        job_storage: DataflowStorageImpl,
        cluster_config: &Vec<cluster::NodeConfig>,
    ) -> Self {
        Coordinator {
            dataflow_storage: job_storage,
            cluster: cluster::Cluster::new(cluster_config),
        }
    }

    pub async fn create_dataflow(&mut self, mut dataflow: Dataflow) -> Result<(), tonic::Status> {
        match dataflow
            .validate()
            .map_err(|err| tonic::Status::invalid_argument(format!("{:?}", err)))
        {
            Ok(_) => {
                self.cluster.partition_dataflow(&mut dataflow);
                let terminate_result = self
                    .terminate_dataflow(dataflow.job_id.as_ref().unwrap())
                    .await;
                if terminate_result.is_err() {
                    return terminate_result.map(|_| ());
                }

                match self.dataflow_storage.save(dataflow.clone()) {
                    Err(err) => return Err(tonic::Status::internal(err.message)),
                    _ => {}
                }

                self.cluster.create_dataflow(&dataflow).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn terminate_dataflow(
        &mut self,
        job_id: &ResourceId,
    ) -> Result<DataflowStatus, tonic::Status> {
        if !self.dataflow_storage.may_exists(job_id) {
            Ok(DataflowStatus::Closed)
        } else {
            match self.dataflow_storage.delete(job_id).map_err(|err| {
                tracing::error!("delete dataflow failed: {:?}", err);
                tonic::Status::internal(err.message)
            }) {
                Ok(_) => self.cluster.terminate_dataflow(job_id).await,
                Err(err) => Err(err),
            }
        }
    }

    pub fn get_dataflow(&self, job_id: &ResourceId) -> Option<Dataflow> {
        self.dataflow_storage.get(job_id)
    }

    pub async fn probe_state(&mut self) {
        self.cluster.probe_state().await
    }
}

#[derive(serde::Deserialize, Clone, Debug)]
pub struct CoordinatorConfig {
    pub port: usize,
    pub cluster: Vec<cluster::NodeConfig>,
    pub storage: DataflowStorageConfig,
}

#[derive(serde::Deserialize, Clone, Debug)]
pub enum DataflowStorageConfig {
    Persist { dataflow_store_path: String },
    Memory,
}

impl DataflowStorageConfig {
    pub fn to_dataflow_storage(&self) -> DataflowStorageImpl {
        match self {
            Self::Persist {
                dataflow_store_path,
            } => DataflowStorageImpl::Persist(PersistDataflowStorage {
                db: sled::open(dataflow_store_path).expect("open rocksdb failed"),
            }),
            Self::Memory => DataflowStorageImpl::Memory(Default::default()),
        }
    }
}

pub struct CoordinatorException {}
