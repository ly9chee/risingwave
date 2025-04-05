// Copyright 2025 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod parquet_file_handler;

mod metrics;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use futures::StreamExt;
use futures_async_stream::{for_await, try_stream};
use iceberg::Catalog;
use iceberg::expr::Predicate as IcebergPredicate;
use iceberg::scan::FileScanTask;
use iceberg::spec::{DataContentType, ManifestList};
use iceberg::table::Table;
use itertools::Itertools;
pub use parquet_file_handler::*;
use risingwave_common::array::arrow::IcebergArrowConvert;
use risingwave_common::array::{ArrayImpl, DataChunk, I64Array, Utf8Array};
use risingwave_common::bail;
use risingwave_common::catalog::{
    ICEBERG_FILE_PATH_COLUMN_NAME, ICEBERG_FILE_POS_COLUMN_NAME, ICEBERG_SEQUENCE_NUM_COLUMN_NAME,
    Schema,
};
use risingwave_common::types::JsonbVal;
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_common_estimate_size::EstimateSize;
use risingwave_pb::batch_plan::iceberg_scan_node::IcebergScanType;
use serde::{Deserialize, Serialize};

pub use self::metrics::{GLOBAL_ICEBERG_SCAN_METRICS, IcebergScanMetrics};
use crate::connector_common::IcebergCommon;
use crate::error::{ConnectorError, ConnectorResult};
use crate::parser::ParserConfig;
use crate::source::{
    BoxSourceChunkStream, Column, SourceContextRef, SourceEnumeratorContextRef, SourceProperties,
    SplitEnumerator, SplitId, SplitMetaData, SplitReader, UnknownFields,
};
pub const ICEBERG_CONNECTOR: &str = "iceberg";

#[derive(Clone, Debug, Deserialize, with_options::WithOptions)]
pub struct IcebergProperties {
    #[serde(flatten)]
    pub common: IcebergCommon,

    // For jdbc catalog
    #[serde(rename = "catalog.jdbc.user")]
    pub jdbc_user: Option<String>,
    #[serde(rename = "catalog.jdbc.password")]
    pub jdbc_password: Option<String>,

    #[serde(flatten)]
    pub unknown_fields: HashMap<String, String>,
}

impl IcebergProperties {
    pub async fn create_catalog(&self) -> ConnectorResult<Arc<dyn Catalog>> {
        let mut java_catalog_props = HashMap::new();
        if let Some(jdbc_user) = self.jdbc_user.clone() {
            java_catalog_props.insert("jdbc.user".to_owned(), jdbc_user);
        }
        if let Some(jdbc_password) = self.jdbc_password.clone() {
            java_catalog_props.insert("jdbc.password".to_owned(), jdbc_password);
        }
        // TODO: support path_style_access and java_catalog_props for iceberg source
        self.common.create_catalog(&java_catalog_props).await
    }

    pub async fn load_table(&self) -> ConnectorResult<Table> {
        let mut java_catalog_props = HashMap::new();
        if let Some(jdbc_user) = self.jdbc_user.clone() {
            java_catalog_props.insert("jdbc.user".to_owned(), jdbc_user);
        }
        if let Some(jdbc_password) = self.jdbc_password.clone() {
            java_catalog_props.insert("jdbc.password".to_owned(), jdbc_password);
        }
        // TODO: support java_catalog_props for iceberg source
        self.common.load_table(&java_catalog_props).await
    }
}

impl SourceProperties for IcebergProperties {
    type Split = IcebergSplit;
    type SplitEnumerator = IcebergSplitEnumerator;
    type SplitReader = IcebergFileReader;

    const SOURCE_NAME: &'static str = ICEBERG_CONNECTOR;
}

impl UnknownFields for IcebergProperties {
    fn unknown_fields(&self) -> HashMap<String, String> {
        self.unknown_fields.clone()
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IcebergFileScanTaskJsonStr(String);

impl IcebergFileScanTaskJsonStr {
    pub fn deserialize(&self) -> FileScanTask {
        serde_json::from_str(&self.0).unwrap()
    }

    pub fn serialize(task: &FileScanTask) -> Self {
        Self(serde_json::to_string(task).unwrap())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IcebergFileScanTask {
    Data(Vec<FileScanTask>),
    EqualityDelete(Vec<FileScanTask>),
    PositionDelete(Vec<FileScanTask>),
    CountStar(u64),
}

impl IcebergFileScanTask {
    pub fn new_count_star(count_sum: u64) -> Self {
        IcebergFileScanTask::CountStar(count_sum)
    }

    pub fn new_scan_with_scan_type(
        iceberg_scan_type: IcebergScanType,
        data_files: Vec<FileScanTask>,
        equality_delete_files: Vec<FileScanTask>,
        position_delete_files: Vec<FileScanTask>,
    ) -> Self {
        match iceberg_scan_type {
            IcebergScanType::EqualityDeleteScan => {
                IcebergFileScanTask::EqualityDelete(equality_delete_files)
            }
            IcebergScanType::DataScan => IcebergFileScanTask::Data(data_files),
            IcebergScanType::PositionDeleteScan => {
                IcebergFileScanTask::PositionDelete(position_delete_files)
            }
            IcebergScanType::Unspecified | IcebergScanType::CountStar => {
                unreachable!("Unspecified iceberg scan type")
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            IcebergFileScanTask::Data(data_files) => data_files.is_empty(),
            IcebergFileScanTask::EqualityDelete(equality_delete_files) => {
                equality_delete_files.is_empty()
            }
            IcebergFileScanTask::PositionDelete(position_delete_files) => {
                position_delete_files.is_empty()
            }
            IcebergFileScanTask::CountStar(_) => false,
        }
    }

    pub fn files(&self) -> Vec<String> {
        match self {
            IcebergFileScanTask::Data(file_scan_tasks)
            | IcebergFileScanTask::EqualityDelete(file_scan_tasks)
            | IcebergFileScanTask::PositionDelete(file_scan_tasks) => file_scan_tasks
                .iter()
                .map(|task| task.data_file_path.clone())
                .collect(),
            IcebergFileScanTask::CountStar(_) => vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IcebergSplit {
    pub split_id: i64,
    // TODO: remove this field. It seems not used.
    pub snapshot_id: i64,
    pub task: IcebergFileScanTask,
}

impl IcebergSplit {
    pub fn empty(iceberg_scan_type: IcebergScanType) -> Self {
        if let IcebergScanType::CountStar = iceberg_scan_type {
            Self {
                split_id: 0,
                snapshot_id: 0,
                task: IcebergFileScanTask::new_count_star(0),
            }
        } else {
            Self {
                split_id: 0,
                snapshot_id: 0,
                task: IcebergFileScanTask::new_scan_with_scan_type(
                    iceberg_scan_type,
                    vec![],
                    vec![],
                    vec![],
                ),
            }
        }
    }
}

impl SplitMetaData for IcebergSplit {
    fn id(&self) -> SplitId {
        self.split_id.to_string().into()
    }

    fn restore_from_json(value: JsonbVal) -> ConnectorResult<Self> {
        serde_json::from_value(value.take()).map_err(|e| anyhow!(e).into())
    }

    fn encode_to_json(&self) -> JsonbVal {
        serde_json::to_value(self.clone()).unwrap().into()
    }

    fn update_offset(&mut self, _last_seen_offset: String) -> ConnectorResult<()> {
        unimplemented!()
    }
}

#[derive(Debug, Clone)]
pub struct IcebergSplitEnumerator {
    config: IcebergProperties,
}

#[async_trait]
impl SplitEnumerator for IcebergSplitEnumerator {
    type Properties = IcebergProperties;
    type Split = IcebergSplit;

    async fn new(
        properties: Self::Properties,
        context: SourceEnumeratorContextRef,
    ) -> ConnectorResult<Self> {
        Ok(Self::new_inner(properties, context))
    }

    async fn list_splits(&mut self) -> ConnectorResult<Vec<Self::Split>> {
        // Like file source, iceberg streaming source has a List Executor and a Fetch Executor,
        // instead of relying on SplitEnumerator on meta.
        // TODO: add some validation logic here.
        Ok(vec![])
    }
}
impl IcebergSplitEnumerator {
    pub fn new_inner(properties: IcebergProperties, _context: SourceEnumeratorContextRef) -> Self {
        Self { config: properties }
    }
}

pub enum IcebergTimeTravelInfo {
    Version(i64),
    TimestampMs(i64),
}

impl IcebergSplitEnumerator {
    pub fn get_snapshot_id(
        table: &Table,
        time_travel_info: Option<IcebergTimeTravelInfo>,
    ) -> ConnectorResult<Option<i64>> {
        let current_snapshot = table.metadata().current_snapshot();
        if current_snapshot.is_none() {
            return Ok(None);
        }

        let snapshot_id = match time_travel_info {
            Some(IcebergTimeTravelInfo::Version(version)) => {
                let Some(snapshot) = table.metadata().snapshot_by_id(version) else {
                    bail!("Cannot find the snapshot id in the iceberg table.");
                };
                snapshot.snapshot_id()
            }
            Some(IcebergTimeTravelInfo::TimestampMs(timestamp)) => {
                let snapshot = table
                    .metadata()
                    .snapshots()
                    .filter(|snapshot| snapshot.timestamp_ms() <= timestamp)
                    .max_by_key(|snapshot| snapshot.timestamp_ms());
                match snapshot {
                    Some(snapshot) => snapshot.snapshot_id(),
                    None => {
                        // convert unix time to human-readable time
                        let time = chrono::DateTime::from_timestamp_millis(timestamp);
                        if time.is_some() {
                            bail!("Cannot find a snapshot older than {}", time.unwrap());
                        } else {
                            bail!("Cannot find a snapshot");
                        }
                    }
                }
            }
            None => {
                assert!(current_snapshot.is_some());
                current_snapshot.unwrap().snapshot_id()
            }
        };
        Ok(Some(snapshot_id))
    }

    pub async fn list_splits_batch(
        &self,
        schema: Schema,
        time_traval_info: Option<IcebergTimeTravelInfo>,
        batch_parallelism: usize,
        iceberg_scan_type: IcebergScanType,
        predicate: IcebergPredicate,
    ) -> ConnectorResult<Vec<IcebergSplit>> {
        if batch_parallelism == 0 {
            bail!("Batch parallelism is 0. Cannot split the iceberg files.");
        }
        let table = self.config.load_table().await?;
        let snapshot_id = Self::get_snapshot_id(&table, time_traval_info)?;
        if snapshot_id.is_none() {
            // If there is no snapshot, we will return a mock `IcebergSplit` with empty files.
            return Ok(vec![IcebergSplit::empty(iceberg_scan_type)]);
        }
        if let IcebergScanType::CountStar = iceberg_scan_type {
            self.list_splits_batch_count_star(&table, snapshot_id.unwrap())
                .await
        } else {
            self.list_splits_batch_scan(
                &table,
                snapshot_id.unwrap(),
                schema,
                batch_parallelism,
                iceberg_scan_type,
                predicate,
            )
            .await
        }
    }

    async fn list_splits_batch_scan(
        &self,
        table: &Table,
        snapshot_id: i64,
        schema: Schema,
        batch_parallelism: usize,
        iceberg_scan_type: IcebergScanType,
        predicate: IcebergPredicate,
    ) -> ConnectorResult<Vec<IcebergSplit>> {
        let schema_names = schema.names();
        let require_names = schema_names
            .iter()
            .filter(|name| {
                name.ne(&ICEBERG_SEQUENCE_NUM_COLUMN_NAME)
                    && name.ne(&ICEBERG_FILE_PATH_COLUMN_NAME)
                    && name.ne(&ICEBERG_FILE_POS_COLUMN_NAME)
            })
            .cloned()
            .collect_vec();

        let table_schema = table.metadata().current_schema();
        tracing::debug!("iceberg_table_schema: {:?}", table_schema);

        let mut position_delete_files = vec![];
        let mut position_delete_files_set = HashSet::new();
        let mut data_files = vec![];
        let mut equality_delete_files = vec![];
        let mut equality_delete_files_set = HashSet::new();
        let scan = table
            .scan()
            .with_filter(predicate)
            .snapshot_id(snapshot_id)
            .with_delete_file_processing_enabled(true)
            .select(require_names)
            .build()
            .map_err(|e| anyhow!(e))?;

        let file_scan_stream = scan.plan_files().await.map_err(|e| anyhow!(e))?;

        #[for_await]
        for task in file_scan_stream {
            let mut task: FileScanTask = task.map_err(|e| anyhow!(e))?;
            for mut delete_file in task.deletes.drain(..) {
                match delete_file.data_file_content {
                    iceberg::spec::DataContentType::Data => {
                        bail!("Data file should not in task deletes");
                    }
                    iceberg::spec::DataContentType::EqualityDeletes => {
                        if equality_delete_files_set.insert(delete_file.data_file_path.clone()) {
                            equality_delete_files.push(delete_file);
                        }
                    }
                    iceberg::spec::DataContentType::PositionDeletes => {
                        if position_delete_files_set.insert(delete_file.data_file_path.clone()) {
                            delete_file.project_field_ids = Vec::default();
                            position_delete_files.push(delete_file);
                        }
                    }
                }
            }
            match task.data_file_content {
                iceberg::spec::DataContentType::Data => {
                    data_files.push(task);
                }
                iceberg::spec::DataContentType::EqualityDeletes => {
                    bail!("Equality delete files should not be in the data files");
                }
                iceberg::spec::DataContentType::PositionDeletes => {
                    bail!("Position delete files should not be in the data files");
                }
            }
        }
        // evenly split the files into splits based on the parallelism.
        let data_files = Self::split_n_vecs(data_files, batch_parallelism);
        let equality_delete_files = Self::split_n_vecs(equality_delete_files, batch_parallelism);
        let position_delete_files = Self::split_n_vecs(position_delete_files, batch_parallelism);

        let splits = data_files
            .into_iter()
            .zip_eq_fast(equality_delete_files.into_iter())
            .zip_eq_fast(position_delete_files.into_iter())
            .enumerate()
            .map(
                |(index, ((data_file, equality_delete_file), position_delete_file))| IcebergSplit {
                    split_id: index as i64,
                    snapshot_id,
                    task: IcebergFileScanTask::new_scan_with_scan_type(
                        iceberg_scan_type,
                        data_file,
                        equality_delete_file,
                        position_delete_file,
                    ),
                },
            )
            .filter(|split| !split.task.is_empty())
            .collect_vec();

        if splits.is_empty() {
            return Ok(vec![IcebergSplit::empty(iceberg_scan_type)]);
        }
        Ok(splits)
    }

    pub async fn list_splits_batch_count_star(
        &self,
        table: &Table,
        snapshot_id: i64,
    ) -> ConnectorResult<Vec<IcebergSplit>> {
        let mut record_counts = 0;
        let manifest_list: ManifestList = table
            .metadata()
            .snapshot_by_id(snapshot_id)
            .unwrap()
            .load_manifest_list(table.file_io(), table.metadata())
            .await
            .map_err(|e| anyhow!(e))?;

        for entry in manifest_list.entries() {
            let manifest = entry
                .load_manifest(table.file_io())
                .await
                .map_err(|e| anyhow!(e))?;
            let mut manifest_entries_stream =
                futures::stream::iter(manifest.entries().iter().filter(|e| e.is_alive()));

            while let Some(manifest_entry) = manifest_entries_stream.next().await {
                let file = manifest_entry.data_file();
                assert_eq!(file.content_type(), DataContentType::Data);
                record_counts += file.record_count();
            }
        }
        let split = IcebergSplit {
            split_id: 0,
            snapshot_id,
            task: IcebergFileScanTask::new_count_star(record_counts),
        };
        Ok(vec![split])
    }

    /// List all files in the snapshot to check if there are deletes.
    pub async fn all_delete_parameters(
        table: &Table,
        snapshot_id: i64,
    ) -> ConnectorResult<(Vec<String>, bool)> {
        let scan = table
            .scan()
            .snapshot_id(snapshot_id)
            .with_delete_file_processing_enabled(true)
            .build()
            .map_err(|e| anyhow!(e))?;
        let file_scan_stream = scan.plan_files().await.map_err(|e| anyhow!(e))?;
        let schema = scan.snapshot().schema(table.metadata())?;
        let mut equality_ids = vec![];
        let mut have_position_delete = false;
        #[for_await]
        for task in file_scan_stream {
            let task: FileScanTask = task.map_err(|e| anyhow!(e))?;
            for delete_file in task.deletes {
                match delete_file.data_file_content {
                    iceberg::spec::DataContentType::Data => {}
                    iceberg::spec::DataContentType::EqualityDeletes => {
                        if equality_ids.is_empty() {
                            equality_ids = delete_file.equality_ids;
                        } else if equality_ids != delete_file.equality_ids {
                            bail!("The schema of iceberg equality delete file must be consistent");
                        }
                    }
                    iceberg::spec::DataContentType::PositionDeletes => {
                        have_position_delete = true;
                    }
                }
            }
        }
        let delete_columns = equality_ids
            .into_iter()
            .map(|id| match schema.name_by_field_id(id) {
                Some(name) => Ok::<std::string::String, ConnectorError>(name.to_owned()),
                None => bail!("Delete field id {} not found in schema", id),
            })
            .collect::<ConnectorResult<Vec<_>>>()?;

        Ok((delete_columns, have_position_delete))
    }

    pub async fn get_delete_parameters(
        &self,
        time_travel_info: Option<IcebergTimeTravelInfo>,
    ) -> ConnectorResult<(Vec<String>, bool)> {
        let table = self.config.load_table().await?;
        let snapshot_id = Self::get_snapshot_id(&table, time_travel_info)?;
        if snapshot_id.is_none() {
            return Ok((vec![], false));
        }
        let snapshot_id = snapshot_id.unwrap();
        Self::all_delete_parameters(&table, snapshot_id).await
    }

    fn split_n_vecs(vecs: Vec<FileScanTask>, split_num: usize) -> Vec<Vec<FileScanTask>> {
        let split_size = vecs.len() / split_num;
        let remaining = vecs.len() % split_num;
        let mut result_vecs = (0..split_num)
            .map(|i| {
                let start = i * split_size;
                let end = (i + 1) * split_size;
                vecs[start..end].to_vec()
            })
            .collect_vec();
        for i in 0..remaining {
            result_vecs[i].push(vecs[split_num * split_size + i].clone());
        }
        result_vecs
    }
}

pub struct IcebergScanOpts {
    pub chunk_size: usize,
    pub need_seq_num: bool,
    pub need_file_path_and_pos: bool,
}

#[try_stream(ok = DataChunk, error = ConnectorError)]
pub async fn scan_task_to_chunk(
    table: Table,
    data_file_scan_task: FileScanTask,
    IcebergScanOpts {
        chunk_size,
        need_seq_num,
        need_file_path_and_pos,
    }: IcebergScanOpts,
    metrics: Option<Arc<IcebergScanMetrics>>,
) {
    let table_name = table.identifier().name().to_owned();

    let mut read_bytes = scopeguard::guard(0, |read_bytes| {
        if let Some(metrics) = metrics {
            metrics
                .iceberg_read_bytes
                .with_guarded_label_values(&[&table_name])
                .inc_by(read_bytes as _);
        }
    });

    let data_file_path = data_file_scan_task.data_file_path.clone();
    let data_sequence_number = data_file_scan_task.sequence_number;

    let reader = table.reader_builder().with_batch_size(chunk_size).build();
    let file_scan_stream = tokio_stream::once(Ok(data_file_scan_task));

    // FIXME: what if the start position is not 0? The logic for index seems not correct.
    let mut record_batch_stream = reader.read(Box::pin(file_scan_stream)).await?.enumerate();

    while let Some((index, record_batch)) = record_batch_stream.next().await {
        let record_batch = record_batch?;

        let mut chunk = IcebergArrowConvert.chunk_from_record_batch(&record_batch)?;
        if need_seq_num {
            let (mut columns, visibility) = chunk.into_parts();
            columns.push(Arc::new(ArrayImpl::Int64(I64Array::from_iter(
                vec![data_sequence_number; visibility.len()],
            ))));
            chunk = DataChunk::from_parts(columns.into(), visibility)
        };
        if need_file_path_and_pos {
            let (mut columns, visibility) = chunk.into_parts();
            columns.push(Arc::new(ArrayImpl::Utf8(Utf8Array::from_iter(
                vec![data_file_path.as_str(); visibility.len()],
            ))));
            let index_start = (index * chunk_size) as i64;
            columns.push(Arc::new(ArrayImpl::Int64(I64Array::from_iter(
                (index_start..(index_start + visibility.len() as i64)).collect::<Vec<i64>>(),
            ))));
            chunk = DataChunk::from_parts(columns.into(), visibility)
        }
        *read_bytes += chunk.estimated_heap_size() as u64;
        yield chunk;
    }
}

#[derive(Debug)]
pub struct IcebergFileReader {}

#[async_trait]
impl SplitReader for IcebergFileReader {
    type Properties = IcebergProperties;
    type Split = IcebergSplit;

    async fn new(
        _props: IcebergProperties,
        _splits: Vec<IcebergSplit>,
        _parser_config: ParserConfig,
        _source_ctx: SourceContextRef,
        _columns: Option<Vec<Column>>,
    ) -> ConnectorResult<Self> {
        unimplemented!()
    }

    fn into_stream(self) -> BoxSourceChunkStream {
        unimplemented!()
    }
}
