mod parser;

use log::trace;

use async_trait::async_trait;
use sqlparser::ast::*;
use sqlparser::dialect::Dialect;

use crate::metastore::{
    table::Table, IdRow, ImportFormat, Index, IndexDef, MetaStoreTable, RowKey, Schema, TableId,
};
use crate::table::{Row, TableValue, TimestampValue};
use crate::CubeError;
use crate::{
    metastore::{Column, ColumnType, MetaStore},
    store::{DataFrame, WALDataStore},
};
use std::sync::Arc;

use crate::queryplanner::{QueryPlan, QueryPlanner};

use crate::cluster::{Cluster, JobEvent};

use crate::metastore::job::JobType;
use crate::queryplanner::query_executor::QueryExecutor;
use crate::sql::parser::CubeStoreParser;
use datafusion::physical_plan::datetime_expressions::string_to_timestamp_nanos;
use datafusion::sql::parser::Statement as DFStatement;
use hex::FromHex;
use itertools::Itertools;
use parser::Statement as CubeStoreStatement;

#[async_trait]
pub trait SqlService: Send + Sync {
    async fn exec_query(&self, query: &str) -> Result<DataFrame, CubeError>;
}

pub struct SqlServiceImpl {
    db: Arc<dyn MetaStore>,
    wal_store: Arc<dyn WALDataStore>,
    query_planner: Arc<dyn QueryPlanner>,
    query_executor: Arc<dyn QueryExecutor>,
    cluster: Arc<dyn Cluster>,
}

impl SqlServiceImpl {
    pub fn new(
        db: Arc<dyn MetaStore>,
        wal_store: Arc<dyn WALDataStore>,
        query_planner: Arc<dyn QueryPlanner>,
        query_executor: Arc<dyn QueryExecutor>,
        cluster: Arc<dyn Cluster>,
    ) -> Arc<SqlServiceImpl> {
        Arc::new(SqlServiceImpl {
            db,
            wal_store,
            query_planner,
            query_executor,
            cluster,
        })
    }

    async fn create_schema(
        &self,
        name: String,
        if_not_exists: bool,
    ) -> Result<IdRow<Schema>, CubeError> {
        self.db.create_schema(name, if_not_exists).await
    }

    async fn create_table(
        &self,
        schema_name: String,
        table_name: String,
        columns: &Vec<ColumnDef>,
        external: bool,
        location: Option<String>,
        indexes: Vec<Statement>,
    ) -> Result<IdRow<Table>, CubeError> {
        let columns_to_set = convert_columns_type(columns)?;
        let mut indexes_to_create = Vec::new();
        for index in indexes.iter() {
            if let Statement::CreateIndex { name, columns, .. } = index {
                indexes_to_create.push(IndexDef {
                    name: name.to_string(),
                    columns: columns.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                });
            }
        }
        if external {
            let listener = self.cluster.job_result_listener();
            let table = self
                .db
                .create_table(
                    schema_name,
                    table_name,
                    columns_to_set,
                    location,
                    Some(ImportFormat::CSV),
                    indexes_to_create,
                )
                .await?;
            listener
                .wait_for_job_result(
                    RowKey::Table(TableId::Tables, table.get_id()),
                    JobType::TableImport,
                )
                .await?;
            let wal_listener = self.cluster.job_result_listener();
            let wals = self.db.get_wals_for_table(table.get_id()).await?;
            let events = wal_listener
                .wait_for_job_results(
                    wals.into_iter()
                        .map(|wal| {
                            (
                                RowKey::Table(TableId::WALs, wal.get_id()),
                                JobType::WalPartitioning,
                            )
                        })
                        .collect(),
                )
                .await?;

            for v in events {
                if let JobEvent::Error(_, _, e) = v {
                    return Err(CubeError::user(format!("Create table failed: {}", e)));
                }
            }

            Ok(table)
        } else {
            self.db
                .create_table(
                    schema_name,
                    table_name,
                    columns_to_set,
                    None,
                    None,
                    indexes_to_create,
                )
                .await
        }
    }

    async fn create_index(
        &self,
        schema_name: String,
        table_name: String,
        name: String,
        columns: &Vec<Ident>,
    ) -> Result<IdRow<Index>, CubeError> {
        Ok(self
            .db
            .create_index(
                schema_name,
                table_name,
                IndexDef {
                    name,
                    columns: columns.iter().map(|c| c.value.to_string()).collect(),
                },
            )
            .await?)
    }

    async fn insert_data<'a>(
        &'a self,
        schema_name: String,
        table_name: String,
        columns: &'a Vec<Ident>,
        data: &'a Vec<Vec<Expr>>,
    ) -> Result<u64, CubeError> {
        let table = self
            .db
            .get_table(schema_name.clone(), table_name.clone())
            .await?;
        let table_columns = table.get_row().clone();
        let table_columns = table_columns.get_columns();
        let mut real_col: Vec<&Column> = Vec::new();
        for column in columns {
            let c = if let Some(item) = table_columns
                .iter()
                .find(|voc| *voc.get_name() == column.value)
            {
                item
            } else {
                return Err(CubeError::user(format!(
                    "Column {} does noot present in table {}.{}.",
                    column.value, schema_name, table_name
                )));
            };
            real_col.push(c);
        }

        let chunk_len = self.wal_store.get_wal_chunk_size();

        let mut wal_ids = Vec::new();

        let listener = self.cluster.job_result_listener();
        for rows_chunk in data.chunks(chunk_len) {
            let data_frame = parse_chunk(rows_chunk, &real_col)?;
            wal_ids.push(
                self.wal_store
                    .add_wal(table.clone(), data_frame)
                    .await?
                    .get_id(),
            );
        }

        let res = listener
            .wait_for_job_results(
                wal_ids
                    .into_iter()
                    .map(|id| (RowKey::Table(TableId::WALs, id), JobType::WalPartitioning))
                    .collect(),
            )
            .await?;

        for v in res {
            if let JobEvent::Error(_, _, e) = v {
                return Err(CubeError::user(format!("Insert job failed: {}", e)));
            }
        }

        Ok(data.len() as u64)
    }
}

#[derive(Debug)]
pub struct MySqlDialectWithBackTicks {}

impl Dialect for MySqlDialectWithBackTicks {
    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        ch == '"' || ch == '`'
    }

    fn is_identifier_start(&self, ch: char) -> bool {
        // See https://dev.mysql.com/doc/refman/8.0/en/identifiers.html.
        // We don't yet support identifiers beginning with numbers, as that
        // makes it hard to distinguish numeric literals.
        (ch >= 'a' && ch <= 'z')
            || (ch >= 'A' && ch <= 'Z')
            || ch == '_'
            || ch == '$'
            || (ch >= '\u{0080}' && ch <= '\u{ffff}')
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        self.is_identifier_start(ch) || (ch >= '0' && ch <= '9')
    }
}

#[async_trait]
impl SqlService for SqlServiceImpl {
    async fn exec_query(&self, q: &str) -> Result<DataFrame, CubeError> {
        if !q.to_lowercase().starts_with("insert") {
            trace!("Query: '{}'", q);
        }
        if let Some(data_frame) = SqlServiceImpl::handle_workbench_queries(q) {
            return Ok(data_frame);
        }
        let ast = {
            let replaced_quote = q.replace("\\'", "''");
            let mut parser = CubeStoreParser::new(&replaced_quote)?;
            parser.parse_statement()?
        };
        // trace!("AST is: {:?}", ast);
        match ast {
            CubeStoreStatement::Statement(Statement::ShowVariable { variable }) => {
                match variable.value.to_lowercase() {
                    s if s == "schemas" => Ok(DataFrame::from(self.db.get_schemas().await?)),
                    s if s == "tables" => Ok(DataFrame::from(self.db.get_tables().await?)),
                    s if s == "chunks" => {
                        Ok(DataFrame::from(self.db.chunks_table().all_rows().await?))
                    }
                    s if s == "indexes" => {
                        Ok(DataFrame::from(self.db.index_table().all_rows().await?))
                    }
                    s if s == "partitions" => {
                        Ok(DataFrame::from(self.db.partition_table().all_rows().await?))
                    }
                    x => Err(CubeError::user(format!("Unknown SHOW: {}", x))),
                }
            }
            CubeStoreStatement::Statement(Statement::SetVariable { .. }) => {
                Ok(DataFrame::new(vec![], vec![]))
            }
            CubeStoreStatement::CreateSchema {
                schema_name,
                if_not_exists,
            } => {
                let name = schema_name.to_string();
                let res = self.create_schema(name, if_not_exists).await?;
                Ok(DataFrame::from(vec![res]))
            }
            CubeStoreStatement::CreateTable {
                create_table:
                    Statement::CreateTable {
                        name,
                        columns,
                        external,
                        location,
                        ..
                    },
                indexes,
            } => {
                let nv = &name.0;
                if nv.len() != 2 {
                    return Err(CubeError::user(format!(
                        "Schema's name should be present in table name but found: {}",
                        name
                    )));
                }
                let schema_name = &nv[0].value;
                let table_name = &nv[1].value;

                let res = self
                    .create_table(
                        schema_name.clone(),
                        table_name.clone(),
                        &columns,
                        external,
                        location,
                        indexes,
                    )
                    .await?;
                Ok(DataFrame::from(vec![res]))
            }
            CubeStoreStatement::Statement(Statement::CreateIndex {
                name,
                table_name,
                columns,
                ..
            }) => {
                if table_name.0.len() != 2 {
                    return Err(CubeError::user(format!(
                        "Schema's name should be present in table name but found: {}",
                        table_name
                    )));
                }
                let schema_name = &table_name.0[0].value;
                let table_name = &table_name.0[1].value;
                let res = self
                    .create_index(
                        schema_name.to_string(),
                        table_name.to_string(),
                        name.to_string(),
                        &columns
                            .iter()
                            .map(|c| -> Result<_, _> {
                                if let Expr::Identifier(ident) = &c.expr {
                                    Ok(ident.clone())
                                } else {
                                    Err(CubeError::user(format!(
                                        "Unsupported column expression in index: {:?}",
                                        c.expr
                                    )))
                                }
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                    .await?;
                Ok(DataFrame::from(vec![res]))
            }
            CubeStoreStatement::Statement(Statement::Drop {
                object_type, names, ..
            }) => {
                match object_type {
                    ObjectType::Schema => {
                        self.db.delete_schema(names[0].to_string()).await?;
                    }
                    ObjectType::Table => {
                        let table = self
                            .db
                            .get_table(names[0].0[0].to_string(), names[0].0[1].to_string())
                            .await?;
                        self.db.drop_table(table.get_id()).await?;
                    }
                    _ => return Err(CubeError::user("Unsupported drop operation".to_string())),
                }
                Ok(DataFrame::new(vec![], vec![]))
            }
            CubeStoreStatement::Statement(Statement::Insert {
                table_name,
                columns,
                source,
            }) => {
                let data = if let SetExpr::Values(Values(data_series)) = &source.body {
                    data_series
                } else {
                    return Err(CubeError::user(format!(
                        "Data should be present in query. Your query was '{}'",
                        q
                    )));
                };

                let nv = &table_name.0;
                if nv.len() != 2 {
                    return Err(CubeError::user(format!("Schema's name should be present in query (boo.table1). Your query was '{}'", q)));
                }
                let schema_name = &nv[0].value;
                let table_name = &nv[1].value;

                self.insert_data(schema_name.clone(), table_name.clone(), &columns, data)
                    .await?;
                Ok(DataFrame::new(vec![], vec![]))
            }
            CubeStoreStatement::Statement(Statement::Query(q)) => {
                let logical_plan = self
                    .query_planner
                    .logical_plan(DFStatement::Statement(Statement::Query(q)))
                    .await?;
                // TODO distribute and combine
                let res = match logical_plan {
                    QueryPlan::Meta(logical_plan) => {
                        self.query_planner.execute_meta_plan(logical_plan).await?
                    }
                    QueryPlan::Select(serialized) => {
                        self.query_executor
                            .execute_router_plan(serialized, self.cluster.clone())
                            .await?
                    }
                };
                Ok(res)
            }
            _ => Err(CubeError::user(format!("Unsupported SQL: '{}'", q))),
        }
    }
}

fn convert_columns_type(columns: &Vec<ColumnDef>) -> Result<Vec<Column>, CubeError> {
    let mut rolupdb_columns = Vec::new();

    for (i, col) in columns.iter().enumerate() {
        let cube_col = Column::new(
            col.name.value.clone(),
            match &col.data_type {
                DataType::Date
                | DataType::Time
                | DataType::Char(_)
                | DataType::Varchar(_)
                | DataType::Clob(_)
                | DataType::Text => ColumnType::String,
                DataType::Uuid
                | DataType::Binary(_)
                | DataType::Varbinary(_)
                | DataType::Blob(_)
                | DataType::Bytea
                | DataType::Array(_) => ColumnType::Bytes,
                DataType::Decimal(precision, scale) => {
                    let mut precision = precision.unwrap_or(18);
                    let mut scale = scale.unwrap_or(5);
                    if precision > 18 {
                        precision = 18;
                    }
                    if scale > 5 {
                        scale = 10;
                    }
                    if scale > precision {
                        precision = scale;
                    }
                    ColumnType::Decimal {
                        precision: precision as i32,
                        scale: scale as i32,
                    }
                }
                DataType::SmallInt | DataType::Int | DataType::BigInt | DataType::Interval => {
                    ColumnType::Int
                }
                DataType::Boolean => ColumnType::Boolean,
                DataType::Float(_) | DataType::Real | DataType::Double => ColumnType::Float,
                DataType::Timestamp => ColumnType::Timestamp,
                DataType::Custom(custom) => {
                    let custom_type_name = custom.to_string().to_lowercase();
                    match custom_type_name.as_str() {
                        "mediumint" => ColumnType::Int,
                        "varbinary" => ColumnType::Bytes,
                        "hyperloglog" => ColumnType::HyperLogLog,
                        _ => return Err(CubeError::user(format!(
                                "Custom type '{}' is not supported",
                                custom)))
                    }
                }
                DataType::Regclass => {
                    return Err(CubeError::user(
                        "Type 'RegClass' is not suppored.".to_string(),
                    ));
                }
            },
            i,
        );
        rolupdb_columns.push(cube_col);
    }
    Ok(rolupdb_columns)
}

fn parse_chunk(chunk: &[Vec<Expr>], column: &Vec<&Column>) -> Result<DataFrame, CubeError> {
    let mut res: Vec<Row> = Vec::new();
    for r in chunk {
        let mut row = vec![TableValue::Int(0); column.len()];
        for i in 0..r.len() {
            row[column[i].get_index()] = extract_data(&r[i], column, i)?;
        }
        res.push(Row::new(row));
    }
    Ok(DataFrame::new(
        column.iter().map(|c| (*c).clone()).collect::<Vec<Column>>(),
        res,
    ))
}

fn decode_byte(s: &str) -> Option<u8> {
    let v = s.as_bytes();
    if v.len() != 2 {
        return None;
    }
    let decode_char = |c| match c {
        b'a'..=b'f' => Some(10 + c - b'a'),
        b'A'..=b'F' => Some(10 + c - b'A'),
        b'0'..=b'9' => Some(c - b'0'),
        _ => None,
    };
    let v0 = decode_char(v[0])?;
    let v1 = decode_char(v[1])?;
    return Some(v0 * 16 + v1);
}

fn parse_hyper_log_log(v: &Value) -> Result<Vec<u8>, CubeError> {
    let bytes = parse_binary_string(v)?;
    // TODO: check without memory allocations. this is run on hot path.
    if let Err(e) = cubehll::HllSketch::read(&bytes) {
        return Err(e.into());
    }
    return Ok(bytes);
}

fn parse_binary_string(v: &Value) -> Result<Vec<u8>, CubeError> {
    match v {
        Value::Number(s) => Ok(s.as_bytes().to_vec()),
        // We interpret strings of the form '0f 0a 14 ff' as a list of hex-encoded bytes.
        // MySQL will store bytes of the string itself instead and we should do the same.
        // TODO: Ensure CubeJS does not send strings of this form our way and match MySQL behavior.
        Value::SingleQuotedString(s) => s
            .split(' ')
            .filter(|b| !b.is_empty())
            .map(|s| {
                decode_byte(s).ok_or_else(|| {
                    CubeError::user(format!("cannot convert value to binary string: {}", v))
                })
            })
            .try_collect(),
        Value::HexStringLiteral(s) => Ok(Vec::from_hex(s.as_bytes())?),
        _ => Err(CubeError::user(format!(
            "cannot convert value to binary string: {}",
            v
        ))),
    }
}

fn extract_data(cell: &Expr, column: &Vec<&Column>, i: usize) -> Result<TableValue, CubeError> {
    if let Expr::Value(Value::Null) = cell {
        return Ok(TableValue::Null);
    }
    let res = {
        match column[i].get_column_type() {
            ColumnType::String => {
                let val = if let Expr::Value(Value::SingleQuotedString(v)) = cell {
                    v
                } else {
                    return Err(CubeError::user(format!(
                        "Single quoted string is expected but {:?} found",
                        cell
                    )));
                };
                TableValue::String(val.to_string())
            }
            ColumnType::Int => {
                let val_int = match cell {
                    Expr::Value(Value::Number(v)) | Expr::Value(Value::SingleQuotedString(v)) => {
                        v.parse::<i64>()
                    }
                    Expr::UnaryOp {
                        op: UnaryOperator::Minus,
                        expr,
                    } => {
                        if let Expr::Value(Value::Number(v)) = expr.as_ref() {
                            v.parse::<i64>().map(|v| v * -1)
                        } else {
                            return Err(CubeError::user(format!(
                                "Can't parse int from, {:?}",
                                cell
                            )));
                        }
                    }
                    _ => return Err(CubeError::user(format!("Can't parse int from, {:?}", cell))),
                };
                if let Err(e) = val_int {
                    return Err(CubeError::user(format!(
                        "Can't parse int from, {:?}: {}",
                        cell, e
                    )));
                }
                TableValue::Int(val_int.unwrap())
            }
            ColumnType::Decimal { .. } => {
                let decimal_val = parse_decimal(cell)?;
                TableValue::Decimal(decimal_val.to_string())
            }
            ColumnType::Bytes => {
                let val;
                if let Expr::Value(v) = cell {
                    val = parse_binary_string(v)
                } else {
                    return Err(CubeError::user("Corrupted data in query.".to_string()));
                };
                return Ok(TableValue::Bytes(val?));
            }
            ColumnType::HyperLogLog => {
                let val;
                if let Expr::Value(v) = cell {
                    val = parse_hyper_log_log(v)
                } else {
                    return Err(CubeError::user("Corrupted data in query.".to_string()));
                };
                return Ok(TableValue::Bytes(val?));
            }
            ColumnType::Timestamp => match cell {
                Expr::Value(Value::SingleQuotedString(v)) => {
                    TableValue::Timestamp(TimestampValue::new(string_to_timestamp_nanos(v)?))
                }
                x => {
                    return Err(CubeError::user(format!(
                        "Can't parse timestamp from, {:?}",
                        x
                    )))
                }
            },
            ColumnType::Boolean => match cell {
                Expr::Value(Value::SingleQuotedString(v)) => {
                    TableValue::Boolean(v.to_lowercase() == "true")
                }
                Expr::Value(Value::Boolean(b)) => TableValue::Boolean(*b),
                x => {
                    return Err(CubeError::user(format!(
                        "Can't parse boolean from, {:?}",
                        x
                    )))
                }
            },
            ColumnType::Float => {
                let decimal_val = parse_decimal(cell)?;
                TableValue::Float(decimal_val.to_string())
            }
        }
    };
    Ok(res)
}

fn parse_decimal(cell: &Expr) -> Result<f64, CubeError> {
    let decimal_val = match cell {
        Expr::Value(Value::Number(v)) | Expr::Value(Value::SingleQuotedString(v)) => {
            v.parse::<f64>()
        }
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => {
            if let Expr::Value(Value::Number(v)) = expr.as_ref() {
                v.parse::<f64>().map(|v| v * -1.0)
            } else {
                return Err(CubeError::user(format!(
                    "Can't parse decimal from, {:?}",
                    cell
                )));
            }
        }
        _ => {
            return Err(CubeError::user(format!(
                "Can't parse decimal from, {:?}",
                cell
            )))
        }
    };
    if let Err(e) = decimal_val {
        return Err(CubeError::user(format!(
            "Can't parse decimal from, {:?}: {}",
            cell, e
        )));
    }
    Ok(decimal_val?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::MockCluster;
    use crate::config::{Config, FileStoreProvider};
    use crate::metastore::RocksMetaStore;
    use crate::queryplanner::query_executor::MockQueryExecutor;
    use crate::queryplanner::MockQueryPlanner;
    use crate::remotefs::LocalDirRemoteFs;
    use crate::store::WALStore;
    use itertools::Itertools;
    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    use rocksdb::{Options, DB};
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::Duration;
    use std::{env, fs};
    use uuid::Uuid;

    #[actix_rt::test]
    async fn create_schema_test() {
        let config = Config::test("create_schema_test");
        let path = "/tmp/test_create_schema";
        let _ = DB::destroy(&Options::default(), path);
        let store_path = path.to_string() + &"_store".to_string();
        let remote_store_path = path.to_string() + &"remote_store".to_string();
        let _ = fs::remove_dir_all(store_path.clone());
        let _ = fs::remove_dir_all(remote_store_path.clone());

        {
            let remote_fs = LocalDirRemoteFs::new(
                PathBuf::from(store_path.clone()),
                PathBuf::from(remote_store_path.clone()),
            );
            let meta_store = RocksMetaStore::new(path, remote_fs.clone(), config.config_obj());
            let store = WALStore::new(meta_store.clone(), remote_fs.clone(), 10);
            let service = SqlServiceImpl::new(
                meta_store,
                store,
                Arc::new(MockQueryPlanner::new()),
                Arc::new(MockQueryExecutor::new()),
                Arc::new(MockCluster::new()),
            );
            let i = service.exec_query("CREATE SCHEMA foo").await.unwrap();
            assert_eq!(
                i.get_rows()[0],
                Row::new(vec![
                    TableValue::Int(1),
                    TableValue::String("foo".to_string())
                ])
            );
        }
        let _ = DB::destroy(&Options::default(), path);
        let _ = fs::remove_dir_all(store_path.clone());
        let _ = fs::remove_dir_all(remote_store_path.clone());
    }

    #[actix_rt::test]
    async fn create_table_test() {
        let config = Config::test("create_table_test");
        let path = "/tmp/test_create_table";
        let _ = DB::destroy(&Options::default(), path);
        let store_path = path.to_string() + &"_store".to_string();
        let remote_store_path = path.to_string() + &"remote_store".to_string();
        let _ = fs::remove_dir_all(store_path.clone());
        let _ = fs::remove_dir_all(remote_store_path.clone());
        {
            let remote_fs = LocalDirRemoteFs::new(
                PathBuf::from(store_path.clone()),
                PathBuf::from(remote_store_path.clone()),
            );
            let meta_store = RocksMetaStore::new(path, remote_fs.clone(), config.config_obj());
            let store = WALStore::new(meta_store.clone(), remote_fs.clone(), 10);
            let service = SqlServiceImpl::new(
                meta_store,
                store,
                Arc::new(MockQueryPlanner::new()),
                Arc::new(MockQueryExecutor::new()),
                Arc::new(MockCluster::new()),
            );
            let i = service.exec_query("CREATE SCHEMA Foo").await.unwrap();
            assert_eq!(
                i.get_rows()[0],
                Row::new(vec![
                    TableValue::Int(1),
                    TableValue::String("Foo".to_string())
                ])
            );
            let query = "CREATE TABLE Foo.Persons (
                                PersonID int,
                                LastName varchar(255),
                                FirstName varchar(255),
                                Address varchar(255),
                                City varchar(255)
                              );";
            let i = service.exec_query(&query.to_string()).await.unwrap();
            assert_eq!(i.get_rows()[0], Row::new(vec![
                TableValue::Int(1),
                TableValue::String("Persons".to_string()),
                TableValue::String("1".to_string()),
                TableValue::String("[{\"name\":\"PersonID\",\"column_type\":\"Int\",\"column_index\":0},{\"name\":\"LastName\",\"column_type\":\"String\",\"column_index\":1},{\"name\":\"FirstName\",\"column_type\":\"String\",\"column_index\":2},{\"name\":\"Address\",\"column_type\":\"String\",\"column_index\":3},{\"name\":\"City\",\"column_type\":\"String\",\"column_index\":4}]".to_string()),
                TableValue::String("NULL".to_string()),
                TableValue::String("NULL".to_string()),
                TableValue::String("false".to_string()),
            ]));
        }
        let _ = DB::destroy(&Options::default(), path);
        let _ = fs::remove_dir_all(store_path.clone());
        let _ = fs::remove_dir_all(remote_store_path.clone());
    }

    #[tokio::test]
    async fn insert() {
        Config::run_test("insert", async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA Foo").await.unwrap();

            let _ = service.exec_query(
                "CREATE TABLE Foo.Persons (
                                PersonID int,
                                LastName varchar(255),
                                FirstName varchar(255),
                                Address varchar(255),
                                City varchar(255)
                              )"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO Foo.Persons
            (
                PersonID,
                LastName,
                FirstName,
                Address,
                City
            )

            VALUES
            (23, 'LastName 1', 'FirstName 1', 'Address 1', 'City 1'), (38, 'LastName 21', 'FirstName 2', 'Address 2', 'City 2'),
            (24, 'LastName 3', 'FirstName 1', 'Address 1', 'City 1'), (37, 'LastName 22', 'FirstName 2', 'Address 2', 'City 2'),
            (25, 'LastName 4', 'FirstName 1', 'Address 1', 'City 1'), (36, 'LastName 23', 'FirstName 2', 'Address 2', 'City 2'),
            (26, 'LastName 5', 'FirstName 1', 'Address 1', 'City 1'), (35, 'LastName 24', 'FirstName 2', 'Address 2', 'City 2'),
            (27, 'LastName 6', 'FirstName 1', 'Address 1', 'City 1'), (34, 'LastName 25', 'FirstName 2', 'Address 2', 'City 2'),
            (28, 'LastName 7', 'FirstName 1', 'Address 1', 'City 1'), (33, 'LastName 26', 'FirstName 2', 'Address 2', 'City 2'),
            (29, 'LastName 8', 'FirstName 1', 'Address 1', 'City 1'), (32, 'LastName 27', 'FirstName 2', 'Address 2', 'City 2'),
            (30, 'LastName 9', 'FirstName 1', 'Address 1', 'City 1'), (31, 'LastName 28', 'FirstName 2', 'Address 2', 'City 2')"
            ).await.unwrap();

            service.exec_query("INSERT INTO Foo.Persons
            (LastName, PersonID, FirstName, Address, City)
            VALUES
            ('LastName 1', 23, 'FirstName 1', 'Address 1', 'City 1'), ('LastName 2', 22, 'FirstName 2', 'Address 2', 'City 2');").await.unwrap();
        }).await;
    }

    #[tokio::test]
    async fn select_test() {
        Config::run_test("select", async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA Foo").await.unwrap();

            let _ = service
                .exec_query(
                    "CREATE TABLE Foo.Persons (
                                PersonID int,
                                LastName varchar(255),
                                FirstName varchar(255),
                                Address varchar(255),
                                City varchar(255)
                              );",
                )
                .await
                .unwrap();

            service
                .exec_query(
                    "INSERT INTO Foo.Persons
                (LastName, PersonID, FirstName, Address, City)
                VALUES
                ('LastName 1', 23, 'FirstName 1', 'Address 1', 'City 1'),
                ('LastName 2', 22, 'FirstName 2', 'Address 2', 'City 2');",
                )
                .await
                .unwrap();

            let result = service
                .exec_query("SELECT PersonID person_id from Foo.Persons")
                .await
                .unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(22)]));
            assert_eq!(result.get_rows()[1], Row::new(vec![TableValue::Int(23)]));
        })
        .await;
    }

    #[tokio::test]
    async fn negative_numbers() {
        Config::run_test("negative_numbers", async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA foo").await.unwrap();

            let _ = service
                .exec_query("CREATE TABLE foo.values (int_value int)")
                .await
                .unwrap();

            service
                .exec_query("INSERT INTO foo.values (int_value) VALUES (-153)")
                .await
                .unwrap();

            let result = service
                .exec_query("SELECT * from foo.values")
                .await
                .unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(-153)]));
        })
        .await;
    }

    #[tokio::test]
    async fn decimal() {
        Config::test("decimal").update_config(|mut c| {
            c.partition_split_threshold = 2;
            c
        }).start_test(async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA foo").await.unwrap();

            let _ = service
                .exec_query("CREATE TABLE foo.values (id int, dec_value decimal, dec_value_1 decimal(18, 2))")
                .await
                .unwrap();

            service
                .exec_query("INSERT INTO foo.values (id, dec_value, dec_value_1) VALUES (1, -153, 1), (2, 20.01, 3.5), (3, 20.30, 12.3), (4, 120.30, 43.12), (5, NULL, NULL), (6, NULL, NULL), (7, NULL, NULL), (NULL, NULL, NULL)")
                .await
                .unwrap();

            let result = service
                .exec_query("SELECT sum(dec_value), sum(dec_value_1) from foo.values")
                .await
                .unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Decimal("7.61".to_string()), TableValue::Decimal("59.92".to_string())]));

            let result = service
                .exec_query("SELECT sum(dec_value), sum(dec_value_1) from foo.values where dec_value > 10")
                .await
                .unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Decimal("160.61".to_string()), TableValue::Decimal("58.92".to_string())]));

            let result = service
                .exec_query("SELECT sum(dec_value), sum(dec_value_1) / 10 from foo.values where dec_value > 10")
                .await
                .unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Decimal("160.61".to_string()), TableValue::Float("5.892".to_string())]));

            let result = service
                .exec_query("SELECT sum(dec_value), sum(dec_value_1) / 10 from foo.values where dec_value_1 < 10")
                .await
                .unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Decimal("-132.99".to_string()), TableValue::Float("0.45".to_string())]));

            let result = service
                .exec_query("SELECT sum(dec_value), sum(dec_value_1) / 10 from foo.values where dec_value_1 < '10'")
                .await
                .unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Decimal("-132.99".to_string()), TableValue::Float("0.45".to_string())]));
        })
            .await;
    }

    #[tokio::test]
    async fn custom_types() {
        Config::run_test("custom_types", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service
                .exec_query("CREATE TABLE foo.values (int_value mediumint)")
                .await
                .unwrap();

            service
                .exec_query("INSERT INTO foo.values (int_value) VALUES (-153)")
                .await
                .unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn group_by_boolean() {
        Config::run_test("group_by_boolean", async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA foo").await.unwrap();

            let _ = service.exec_query("CREATE TABLE foo.bool_group (bool_value boolean)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.bool_group (bool_value) VALUES (true), (false), (true), (false), (false)"
            ).await.unwrap();

            // TODO compaction fails the test in between?
            // service.exec_query(
            //     "INSERT INTO foo.bool_group (bool_value) VALUES (true), (false), (true), (false), (false)"
            // ).await.unwrap();

            let result = service.exec_query("SELECT count(*) from foo.bool_group").await.unwrap();
            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(5)]));

            let result = service.exec_query("SELECT count(*) from foo.bool_group where bool_value = true").await.unwrap();
            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(2)]));

            let result = service.exec_query("SELECT count(*) from foo.bool_group where bool_value = 'true'").await.unwrap();
            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(2)]));

            let result = service.exec_query("SELECT g.bool_value, count(*) from foo.bool_group g GROUP BY 1 ORDER BY 2 DESC").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Boolean(false), TableValue::Int(3)]));
            assert_eq!(result.get_rows()[1], Row::new(vec![TableValue::Boolean(true), TableValue::Int(2)]));
        }).await;
    }

    #[tokio::test]
    async fn group_by_decimal() {
        Config::run_test("group_by_decimal", async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA foo").await.unwrap();

            let _ = service.exec_query("CREATE TABLE foo.decimal_group (id INT, decimal_value DECIMAL)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.decimal_group (id, decimal_value) VALUES (1, 100), (2, 200), (3, 100), (4, 100), (5, 200)"
            ).await.unwrap();

            let result = service.exec_query("SELECT count(*) from foo.decimal_group").await.unwrap();
            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(5)]));

            let result = service.exec_query("SELECT count(*) from foo.decimal_group where decimal_value = 200").await.unwrap();
            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(2)]));

            let result = service.exec_query("SELECT g.decimal_value, count(*) from foo.decimal_group g GROUP BY 1 ORDER BY 2 DESC").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Decimal("100".to_string()), TableValue::Int(3)]));
            assert_eq!(result.get_rows()[1], Row::new(vec![TableValue::Decimal("200".to_string()), TableValue::Int(2)]));
        }).await;
    }

    #[tokio::test]
    async fn float_decimal_scale() {
        Config::run_test("float_decimal_scale", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();
            service.exec_query("CREATE TABLE foo.decimal_group (id INT, decimal_value FLOAT)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.decimal_group (id, decimal_value) VALUES (1, 677863988852), (2, 677863988852.123e-10), (3, 6778639882.123e+3)"
            ).await.unwrap();

            let result = service.exec_query(
                "SELECT SUM(decimal_value) FROM foo.decimal_group"
            ).await.unwrap();

            assert_eq!(result.get_rows(), &vec![Row::new(vec![TableValue::Float("7456503871042.786".to_string())])]);
        }).await;
    }

    #[tokio::test]
    async fn over_2k_booleans() {
        Config::test("over_2k_booleans").update_config(|mut c| {
            c.partition_split_threshold = 1000000;
            c.compaction_chunks_count_threshold = 0;
            c
        }).start_test(async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA foo").await.unwrap();

            let _ = service.exec_query("CREATE TABLE foo.bool_group (bool_value boolean)").await.unwrap();

            for batch in 0..25 {
                let mut bools = Vec::new();
                for i in 0..1000 {
                    bools.push(i % (batch + 1) == 0);
                }

                let values = bools.into_iter().map(|b| format!("({})", b)).join(", ");
                service.exec_query(
                    &format!("INSERT INTO foo.bool_group (bool_value) VALUES {}", values)
                ).await.unwrap();
            }

            let result = service.exec_query("SELECT count(*) from foo.bool_group").await.unwrap();
            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(25000)]));

            let result = service.exec_query("SELECT count(*) from foo.bool_group where bool_value = true").await.unwrap();
            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(3823)]));

            let result = service.exec_query("SELECT g.bool_value, count(*) from foo.bool_group g GROUP BY 1 ORDER BY 2 DESC").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Boolean(false), TableValue::Int(21177)]));
            assert_eq!(result.get_rows()[1], Row::new(vec![TableValue::Boolean(true), TableValue::Int(3823)]));
        }).await;
    }

    #[tokio::test]
    async fn over_10k_join() {
        Config::test("over_10k_join").update_config(|mut c| {
            c.partition_split_threshold = 1000000;
            c.compaction_chunks_count_threshold = 50;
            c
        }).start_test(async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.orders (amount int, email text)").await.unwrap();

            service.exec_query("CREATE INDEX orders_by_email ON foo.orders (email)").await.unwrap();

            service.exec_query("CREATE TABLE foo.customers (email text, system text, uuid text)").await.unwrap();

            service.exec_query("CREATE INDEX customers_by_email ON foo.customers (email)").await.unwrap();

            let mut join_results = Vec::new();

            for batch in 0..25 {
                let mut orders = Vec::new();
                let mut customers = Vec::new();
                for i in 0..1000 {
                    let email = String::from_utf8(thread_rng()
                        .sample_iter(&Alphanumeric)
                        .take(5)
                        .collect()
                    ).unwrap();
                    let domain = String::from_utf8(thread_rng()
                        .sample_iter(&Alphanumeric)
                        .take(5)
                        .collect()
                    ).unwrap();
                    let email = format!("{}@{}.com", email, domain);
                    orders.push((i, email.clone()));
                    if i % (batch + 1) == 0 {
                        let uuid = Uuid::new_v4().to_string();
                        customers.push((email.clone(), uuid.clone()));
                        if i % (batch + 1 + 10) == 0 {
                            customers.push((email.clone(), uuid.clone()));
                            join_results.push(Row::new(vec![TableValue::String(email.clone()), TableValue::String(uuid), TableValue::Int(i * 2)]))
                        } else {
                            join_results.push(Row::new(vec![TableValue::String(email.clone()), TableValue::String(uuid), TableValue::Int(i)]))
                        }
                    } else {
                        join_results.push(Row::new(vec![TableValue::String(email.clone()), TableValue::String("".to_string()), TableValue::Int(i)]))
                    }
                }

                let values = orders.into_iter().map(|(amount, email)| format!("({}, '{}')", amount, email)).join(", ");

                service.exec_query(
                    &format!("INSERT INTO foo.orders (amount, email) VALUES {}", values)
                ).await.unwrap();

                let values = customers.into_iter().map(|(email, uuid)| format!("('{}', 'system', '{}')", email, uuid)).join(", ");

                service.exec_query(
                    &format!("INSERT INTO foo.customers (email, system, uuid) VALUES {}", values)
                ).await.unwrap();
            }

            join_results.sort_by(|a, b| a.sort_key(1).cmp(&b.sort_key(1)));

            let result = service.exec_query("SELECT o.email, c.uuid, sum(o.amount) from foo.orders o LEFT JOIN foo.customers c ON o.email = c.email GROUP BY 1, 2 ORDER BY 1 ASC").await.unwrap();

            assert_eq!(result.get_rows().len(), join_results.len());
            for i in 0..result.get_rows().len() {
                // println!("Actual {}: {:?}", i, &result.get_rows()[i]);
                // println!("Expected {}: {:?}", i, &join_results[i]);
                assert_eq!(&result.get_rows()[i], &join_results[i]);
            }
        }).await;
    }

    #[tokio::test]
    async fn high_frequency_inserts() {
        Config::test("high_frequency_inserts")
            .update_config(|mut c| {
                c.partition_split_threshold = 1000000;
                c.compaction_chunks_count_threshold = 100;
                c
            })
            .start_test(async move |services| {
                let service = services.sql_service;

                service.exec_query("CREATE SCHEMA foo").await.unwrap();

                service
                    .exec_query("CREATE TABLE foo.numbers (num int)")
                    .await
                    .unwrap();

                for i in 0..300 {
                    service
                        .exec_query(&format!("INSERT INTO foo.numbers (num) VALUES ({})", i))
                        .await
                        .unwrap();
                }

                let result = service
                    .exec_query("SELECT count(*) from foo.numbers")
                    .await
                    .unwrap();
                assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(300)]));

                let result = service
                    .exec_query("SELECT sum(num) from foo.numbers")
                    .await
                    .unwrap();
                assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(44850)]));
            })
            .await;
    }

    #[tokio::test]
    async fn inactive_partitions_cleanup() {
        Config::test("inactive_partitions_cleanup")
            .update_config(|mut c| {
                c.partition_split_threshold = 1000000;
                c.compaction_chunks_count_threshold = 0;
                c
            })
            .start_test(async move |services| {
                let service = services.sql_service;

                service.exec_query("CREATE SCHEMA foo").await.unwrap();

                service
                    .exec_query("CREATE TABLE foo.numbers (num int)")
                    .await
                    .unwrap();

                for i in 0..10_u64 {
                    service
                        .exec_query(&format!("INSERT INTO foo.numbers (num) VALUES ({})", i))
                        .await
                        .unwrap();
                }

                // let listener = services.cluster.job_result_listener();
                // let active_partitions = services
                //     .meta_store
                //     .get_active_partitions_by_index_id(1)
                //     .await
                //     .unwrap();
                // let mut last_active_partition = active_partitions.iter().next().unwrap();
                // listener
                //     .wait_for_job_results(vec![(
                //         RowKey::Table(TableId::Partitions, last_active_partition.get_id()),
                //         JobType::Repartition,
                //     )])
                //     .await
                //     .unwrap();

                // TODO API to wait for all jobs to be completed and all events processed
                tokio::time::delay_for(Duration::from_millis(500)).await;

                let result = service
                    .exec_query("SELECT count(*) from foo.numbers")
                    .await
                    .unwrap();
                assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(10)]));

                let active_partitions = services
                    .meta_store
                    .get_active_partitions_by_index_id(1)
                    .await
                    .unwrap();
                let last_active_partition = active_partitions.iter().next().unwrap();

                let files = services
                    .remote_fs
                    .list("")
                    .await
                    .unwrap()
                    .into_iter()
                    .filter(|r| r.ends_with(".parquet"))
                    .collect::<Vec<_>>();
                assert_eq!(
                    files,
                    vec![format!("{}.parquet", last_active_partition.get_id())]
                )
            })
            .await
    }

    #[tokio::test]
    async fn join() {
        Config::run_test("join", async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA foo").await.unwrap();

            let _ = service.exec_query("CREATE TABLE foo.orders (customer_id text, amount int)").await.unwrap();
            let _ = service.exec_query("CREATE TABLE foo.customers (id text, city text, state text)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders (customer_id, amount) VALUES ('a', 10), ('b', 2), ('b', 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (id, city, state) VALUES ('a', 'San Francisco', 'CA'), ('b', 'New York', 'NY')"
            ).await.unwrap();

            let result = service.exec_query("SELECT c.city, sum(o.amount) from foo.orders o JOIN foo.customers c ON o.customer_id = c.id GROUP BY 1 ORDER BY 2 DESC").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::String("San Francisco".to_string()), TableValue::Int(10)]));
            assert_eq!(result.get_rows()[1], Row::new(vec![TableValue::String("New York".to_string()), TableValue::Int(5)]));
        }).await;
    }

    #[tokio::test]
    async fn three_tables_join() {
        Config::run_test("three_tables_join", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.orders (orders_customer_id text, orders_product_id int, amount int)").await.unwrap();
            service.exec_query("CREATE INDEX orders_by_product ON foo.orders (orders_product_id)").await.unwrap();
            service.exec_query("CREATE TABLE foo.customers (customer_id text, city text, state text)").await.unwrap();
            service.exec_query("CREATE TABLE foo.products (product_id int, name text)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders (orders_customer_id, orders_product_id, amount) VALUES ('a', 1, 10), ('b', 2, 2), ('b', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders (orders_customer_id, orders_product_id, amount) VALUES ('b', 1, 10), ('c', 2, 2), ('c', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders (orders_customer_id, orders_product_id, amount) VALUES ('c', 1, 10), ('d', 2, 2), ('d', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (customer_id, city, state) VALUES ('a', 'San Francisco', 'CA'), ('b', 'New York', 'NY')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (customer_id, city, state) VALUES ('c', 'San Francisco', 'CA'), ('d', 'New York', 'NY')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.products (product_id, name) VALUES (1, 'Potato'), (2, 'Tomato')"
            ).await.unwrap();

            let result = service.exec_query(
                "SELECT city, name, sum(amount) FROM foo.orders o \
                LEFT JOIN foo.customers c ON orders_customer_id = customer_id \
                LEFT JOIN foo.products p ON orders_product_id = product_id \
                GROUP BY 1, 2 ORDER BY 3 DESC, 1 ASC, 2 ASC"
            ).await.unwrap();

            let expected = vec![
                Row::new(vec![TableValue::String("San Francisco".to_string()), TableValue::String("Potato".to_string()), TableValue::Int(20)]),
                Row::new(vec![TableValue::String("New York".to_string()), TableValue::String("Potato".to_string()), TableValue::Int(10)]),
                Row::new(vec![TableValue::String("New York".to_string()), TableValue::String("Tomato".to_string()), TableValue::Int(10)]),
                Row::new(vec![TableValue::String("San Francisco".to_string()), TableValue::String("Tomato".to_string()), TableValue::Int(5)])
            ];

            assert_eq!(
                result.get_rows(),
                &expected
            );

        }).await;
    }

    #[tokio::test]
    async fn three_tables_join_with_filter() {
        Config::run_test("three_tables_join_with_filter", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.orders (orders_customer_id text, orders_product_id int, amount int)").await.unwrap();
            service.exec_query("CREATE INDEX orders_by_product ON foo.orders (orders_product_id)").await.unwrap();
            service.exec_query("CREATE TABLE foo.customers (customer_id text, city text, state text)").await.unwrap();
            service.exec_query("CREATE TABLE foo.products (product_id int, name text)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders (orders_customer_id, orders_product_id, amount) VALUES ('a', 1, 10), ('b', 2, 2), ('b', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders (orders_customer_id, orders_product_id, amount) VALUES ('b', 1, 10), ('c', 2, 2), ('c', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders (orders_customer_id, orders_product_id, amount) VALUES ('c', 1, 10), ('d', 2, 2), ('d', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (customer_id, city, state) VALUES ('a', 'San Francisco', 'CA'), ('b', 'New York', 'NY')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (customer_id, city, state) VALUES ('c', 'San Francisco', 'CA'), ('d', 'New York', 'NY')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.products (product_id, name) VALUES (1, 'Potato'), (2, 'Tomato')"
            ).await.unwrap();

            let result = service.exec_query(
                "SELECT city, name, sum(amount) FROM foo.orders o \
                LEFT JOIN foo.products p ON orders_product_id = product_id \
                LEFT JOIN foo.customers c ON orders_customer_id = customer_id \
                WHERE customer_id = 'a' \
                GROUP BY 1, 2 ORDER BY 3 DESC, 1 ASC, 2 ASC"
            ).await.unwrap();

            let expected = vec![
                Row::new(vec![TableValue::String("San Francisco".to_string()), TableValue::String("Potato".to_string()), TableValue::Int(10)]),
            ];

            assert_eq!(
                result.get_rows(),
                &expected
            );

        }).await;
    }

    #[tokio::test]
    async fn three_tables_join_with_union() {
        Config::run_test("three_tables_join_with_union", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.orders_1 (orders_customer_id text, orders_product_id int, amount int)").await.unwrap();
            service.exec_query("CREATE TABLE foo.orders_2 (orders_customer_id text, orders_product_id int, amount int)").await.unwrap();
            service.exec_query("CREATE INDEX orders_by_product_1 ON foo.orders_1 (orders_product_id)").await.unwrap();
            service.exec_query("CREATE INDEX orders_by_product_2 ON foo.orders_2 (orders_product_id)").await.unwrap();
            service.exec_query("CREATE TABLE foo.customers (customer_id text, city text, state text)").await.unwrap();
            service.exec_query("CREATE TABLE foo.products (product_id int, name text)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders_1 (orders_customer_id, orders_product_id, amount) VALUES ('a', 1, 10), ('b', 2, 2), ('b', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders_1 (orders_customer_id, orders_product_id, amount) VALUES ('b', 1, 10), ('c', 2, 2), ('c', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders_2 (orders_customer_id, orders_product_id, amount) VALUES ('c', 1, 10), ('d', 2, 2), ('d', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (customer_id, city, state) VALUES ('a', 'San Francisco', 'CA'), ('b', 'New York', 'NY')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (customer_id, city, state) VALUES ('c', 'San Francisco', 'CA'), ('d', 'New York', 'NY')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.products (product_id, name) VALUES (1, 'Potato'), (2, 'Tomato')"
            ).await.unwrap();

            let result = service.exec_query(
                "SELECT city, name, sum(amount) FROM (SELECT * FROM foo.orders_1 UNION ALL SELECT * FROM foo.orders_2) o \
                LEFT JOIN foo.customers c ON orders_customer_id = customer_id \
                LEFT JOIN foo.products p ON orders_product_id = product_id \
                WHERE customer_id = 'a' \
                GROUP BY 1, 2 ORDER BY 3 DESC, 1 ASC, 2 ASC"
            ).await.unwrap();

            let expected = vec![
                Row::new(vec![TableValue::String("San Francisco".to_string()), TableValue::String("Potato".to_string()), TableValue::Int(10)]),
            ];

            assert_eq!(
                result.get_rows(),
                &expected
            );

        }).await;
    }

    #[tokio::test]
    async fn cluster() {
        Config::test("cluster_router").update_config(|mut config| {
            config.select_workers = vec!["127.0.0.1:4306".to_string(), "127.0.0.1:4307".to_string()];
            config
        }).start_test(async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.orders_1 (orders_customer_id text, orders_product_id int, amount int)").await.unwrap();
            service.exec_query("CREATE TABLE foo.orders_2 (orders_customer_id text, orders_product_id int, amount int)").await.unwrap();
            service.exec_query("CREATE INDEX orders_by_product_1 ON foo.orders_1 (orders_product_id)").await.unwrap();
            service.exec_query("CREATE INDEX orders_by_product_2 ON foo.orders_2 (orders_product_id)").await.unwrap();
            service.exec_query("CREATE TABLE foo.customers (customer_id text, city text, state text)").await.unwrap();
            service.exec_query("CREATE TABLE foo.products (product_id int, name text)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders_1 (orders_customer_id, orders_product_id, amount) VALUES ('a', 1, 10), ('b', 2, 2), ('b', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders_1 (orders_customer_id, orders_product_id, amount) VALUES ('b', 1, 10), ('c', 2, 2), ('c', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders_2 (orders_customer_id, orders_product_id, amount) VALUES ('c', 1, 10), ('d', 2, 2), ('d', 2, 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (customer_id, city, state) VALUES ('a', 'San Francisco', 'CA'), ('b', 'New York', 'NY')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (customer_id, city, state) VALUES ('c', 'San Francisco', 'CA'), ('d', 'New York', 'NY')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.products (product_id, name) VALUES (1, 'Potato'), (2, 'Tomato')"
            ).await.unwrap();

            Config::test("cluster_worker_1").update_config(|mut config| {
                config.worker_bind_address = Some("127.0.0.1:4306".to_string());
                config.store_provider = FileStoreProvider::Filesystem {
                    remote_dir: env::current_dir()
                        .unwrap()
                        .join("cluster_router-upstream".to_string()),
                };
                config
            }).start_test_worker(async move |_| {
                Config::test("cluster_worker_2").update_config(|mut config| {
                    config.worker_bind_address = Some("127.0.0.1:4307".to_string());
                    config.store_provider = FileStoreProvider::Filesystem {
                        remote_dir: env::current_dir()
                            .unwrap()
                            .join("cluster_router-upstream".to_string()),
                    };
                    config
                }).start_test_worker(async move |_| {
                    let result = service.exec_query(
                        "SELECT city, name, sum(amount) FROM (SELECT * FROM foo.orders_1 UNION ALL SELECT * FROM foo.orders_2) o \
                LEFT JOIN foo.customers c ON orders_customer_id = customer_id \
                LEFT JOIN foo.products p ON orders_product_id = product_id \
                WHERE customer_id = 'a' \
                GROUP BY 1, 2 ORDER BY 3 DESC, 1 ASC, 2 ASC"
                    ).await.unwrap();

                    let expected = vec![
                        Row::new(vec![TableValue::String("San Francisco".to_string()), TableValue::String("Potato".to_string()), TableValue::Int(10)]),
                    ];

                    assert_eq!(
                        result.get_rows(),
                        &expected
                    );
                }).await;
            }).await;
        }).await;
    }

    #[tokio::test]
    async fn in_list() {
        Config::run_test("in_list", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.customers (id text, city text, state text)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.customers (id, city, state) VALUES ('a', 'San Francisco', 'CA'), ('b', 'New York', 'NY'), ('c', 'San Diego', 'CA'), ('d', 'Austin', 'TX')"
            ).await.unwrap();

            let result = service.exec_query("SELECT count(*) from foo.customers WHERE state in ('CA', 'TX')").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(3)]));
        }).await;
    }

    #[tokio::test]
    async fn numeric_cast() {
        Config::run_test("numeric_cast", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.managers (id text, department_id int)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.managers (id, department_id) VALUES ('a', 1), ('b', 3), ('c', 3), ('d', 5)"
            ).await.unwrap();

            let result = service.exec_query("SELECT count(*) from foo.managers WHERE department_id in ('3', '5')").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(3)]));
        }).await;
    }

    #[tokio::test]
    async fn union() {
        Config::run_test("union", async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA foo").await.unwrap();

            let _ = service.exec_query("CREATE TABLE foo.orders1 (customer_id text, amount int)").await.unwrap();
            let _ = service.exec_query("CREATE TABLE foo.orders2 (customer_id text, amount int)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders1 (customer_id, amount) VALUES ('a', 10), ('b', 2), ('b', 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.orders2 (customer_id, amount) VALUES ('b', 20), ('c', 20), ('b', 30)"
            ).await.unwrap();

            let result = service.exec_query(
                "SELECT `u`.customer_id, sum(`u`.amount) FROM \
                (select * from foo.orders1 union all select * from foo.orders2) `u` \
                WHERE `u`.customer_id like '%' GROUP BY 1 ORDER BY 2 DESC"
            ).await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::String("b".to_string()), TableValue::Int(55)]));
            assert_eq!(result.get_rows()[1], Row::new(vec![TableValue::String("c".to_string()), TableValue::Int(20)]));
            assert_eq!(result.get_rows()[2], Row::new(vec![TableValue::String("a".to_string()), TableValue::Int(10)]));
        }).await;
    }

    #[tokio::test]
    async fn timestamp_select() {
        Config::run_test("timestamp_select", async move |services| {
            let service = services.sql_service;

            let _ = service.exec_query("CREATE SCHEMA foo").await.unwrap();

            let _ = service.exec_query("CREATE TABLE foo.timestamps (t timestamp)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.timestamps (t) VALUES ('2020-01-01T00:00:00.000Z'), ('2020-01-02T00:00:00.000Z'), ('2020-01-03T00:00:00.000Z')"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.timestamps (t) VALUES ('2020-01-01T00:00:00.000Z'), ('2020-01-02T00:00:00.000Z'), ('2020-01-03T00:00:00.000Z')"
            ).await.unwrap();

            let result = service.exec_query("SELECT count(*) from foo.timestamps WHERE t >= to_timestamp('2020-01-02T00:00:00.000Z')").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(4)]));
        }).await;
    }

    #[tokio::test]
    async fn column_escaping() {
        Config::run_test("column_escaping", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service
                .exec_query("CREATE TABLE foo.timestamps (t timestamp, amount int)")
                .await
                .unwrap();

            service
                .exec_query(
                    "INSERT INTO foo.timestamps (t, amount) VALUES \
                ('2020-01-01T00:00:00.000Z', 1), \
                ('2020-01-01T00:01:00.000Z', 2), \
                ('2020-01-02T00:10:00.000Z', 3)",
                )
                .await
                .unwrap();

            let result = service
                .exec_query(
                    "SELECT date_trunc('day', `timestamp`.t) `day`, sum(`timestamp`.amount) \
                FROM foo.timestamps `timestamp` \
                WHERE `timestamp`.t >= to_timestamp('2020-01-02T00:00:00.000Z') GROUP BY 1",
                )
                .await
                .unwrap();

            assert_eq!(
                result.get_rows()[0],
                Row::new(vec![
                    TableValue::Timestamp(TimestampValue::new(1577923200000000000)),
                    TableValue::Int(3)
                ])
            );
        })
        .await;
    }

    #[tokio::test]
    async fn case_column_escaping() {
        Config::run_test("case_column_escaping", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query(
                "CREATE TABLE foo.timestamps (t timestamp, amount int)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.timestamps (t, amount) VALUES \
                ('2020-01-01T00:00:00.000Z', 1), \
                ('2020-01-01T00:01:00.000Z', 2), \
                ('2020-01-02T00:10:00.000Z', 3)"
            ).await.unwrap();

            let result = service.exec_query(
                "SELECT date_trunc('day', `timestamp`.t) `day`, sum(CASE WHEN `timestamp`.t > to_timestamp('2020-01-02T00:01:00.000Z') THEN `timestamp`.amount END) \
                FROM foo.timestamps `timestamp` \
                WHERE `timestamp`.t >= to_timestamp('2020-01-02T00:00:00.000Z') GROUP BY 1"
            ).await.unwrap();

            assert_eq!(
                result.get_rows()[0],
                Row::new(vec![
                    TableValue::Timestamp(TimestampValue::new(1577923200000000000)),
                    TableValue::Int(3)
                ])
            );
        }).await;
    }

    #[tokio::test]
    async fn inner_column_escaping() {
        Config::run_test("inner_column_escaping", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service
                .exec_query("CREATE TABLE foo.timestamps (t timestamp, amount int)")
                .await
                .unwrap();

            service
                .exec_query(
                    "INSERT INTO foo.timestamps (t, amount) VALUES \
                ('2020-01-01T00:00:00.000Z', 1), \
                ('2020-01-01T00:01:00.000Z', 2), \
                ('2020-01-02T00:10:00.000Z', 3)",
                )
                .await
                .unwrap();

            let result = service
                .exec_query(
                    "SELECT date_trunc('day', `t`) `day`, sum(`amount`) \
                FROM foo.timestamps `timestamp` \
                WHERE `t` >= to_timestamp('2020-01-02T00:00:00.000Z') GROUP BY 1",
                )
                .await
                .unwrap();

            assert_eq!(
                result.get_rows()[0],
                Row::new(vec![
                    TableValue::Timestamp(TimestampValue::new(1577923200000000000)),
                    TableValue::Int(3)
                ])
            );
        })
        .await;
    }

    #[tokio::test]
    async fn create_schema_if_not_exists() {
        Config::run_test("create_schema_if_not_exists", async move |services| {
            let service = services.sql_service;

            let _ = service
                .exec_query("CREATE SCHEMA IF NOT EXISTS Foo")
                .await
                .unwrap();
            let _ = service
                .exec_query("CREATE SCHEMA IF NOT EXISTS Foo")
                .await
                .unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn create_index_before_ingestion() {
        Config::run_test("create_index_before_ingestion", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.timestamps (id int, t timestamp)").await.unwrap();

            service.exec_query("CREATE INDEX by_timestamp ON foo.timestamps (`t`)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.timestamps (id, t) VALUES (1, '2020-01-01T00:00:00.000Z'), (2, '2020-01-02T00:00:00.000Z'), (3, '2020-01-03T00:00:00.000Z')"
            ).await.unwrap();

            let result = service.exec_query("SELECT count(*) from foo.timestamps WHERE t >= to_timestamp('2020-01-02T00:00:00.000Z')").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(2)]));
        }).await;
    }

    #[tokio::test]
    async fn ambiguous_join_sort() {
        Config::run_test("ambiguous_join_sort", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.sessions (t timestamp, id int)").await.unwrap();
            service.exec_query("CREATE TABLE foo.page_views (session_id int, page_view_count int)").await.unwrap();

            service.exec_query("CREATE INDEX by_id ON foo.sessions (id)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.sessions (t, id) VALUES ('2020-01-01T00:00:00.000Z', 1), ('2020-01-02T00:00:00.000Z', 2), ('2020-01-03T00:00:00.000Z', 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.page_views (session_id, page_view_count) VALUES (1, 10), (2, 20), (3, 30)"
            ).await.unwrap();

            let result = service.exec_query("SELECT sum(p.page_view_count) from foo.sessions s JOIN foo.page_views p ON s.id = p.session_id WHERE s.t >= to_timestamp('2020-01-02T00:00:00.000Z')").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(50)]));
        }).await;
    }

    #[tokio::test]
    async fn join_with_aliases() {
        Config::run_test("join_with_aliases", async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.sessions (t timestamp, id int)").await.unwrap();
            service.exec_query("CREATE TABLE foo.page_views (session_id int, page_view_count int)").await.unwrap();

            service.exec_query("CREATE INDEX by_id ON foo.sessions (id)").await.unwrap();

            service.exec_query(
                "INSERT INTO foo.sessions (t, id) VALUES ('2020-01-01T00:00:00.000Z', 1), ('2020-01-02T00:00:00.000Z', 2), ('2020-01-03T00:00:00.000Z', 3)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.page_views (session_id, page_view_count) VALUES (1, 10), (2, 20), (3, 30)"
            ).await.unwrap();

            let result = service.exec_query("SELECT sum(`page_view_count`) from foo.sessions `sessions` JOIN foo.page_views `page_views` ON `id` = `session_id` WHERE `t` >= to_timestamp('2020-01-02T00:00:00.000Z')").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(50)]));
        }).await;
    }

    #[tokio::test]
    async fn create_table_with_location() {
        Config::run_test("create_table_with_location", async move |services| {
            let service = services.sql_service;

            let path = {
                let mut dir = env::temp_dir();
                dir.push("foo.csv");

                let mut file = File::create(dir.clone()).unwrap();

                file.write_all("1,San Francisco\n".as_bytes()).unwrap();
                file.write_all("2,New York\n".as_bytes()).unwrap();

                dir
            };

            let _ = service.exec_query("CREATE SCHEMA IF NOT EXISTS Foo").await.unwrap();
            let _ = service.exec_query(&format!("CREATE TABLE Foo.Persons (id int, city text) INDEX persons_city (city, id) LOCATION '{}'", path.as_os_str().to_string_lossy())).await.unwrap();
            let res = service.exec_query("CREATE INDEX by_city ON Foo.Persons (city)").await;
            let error = format!("{:?}", res);
            assert!(error.contains("has data"));

            let result = service.exec_query("SELECT count(*) as cnt from Foo.Persons").await.unwrap();
            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(2)]));
        }).await;
    }

    #[tokio::test]
    async fn bytes() {
        Config::run_test("bytes", async move |services| {
            let service = services.sql_service;

            let _ = service
                .exec_query("CREATE SCHEMA IF NOT EXISTS s")
                .await
                .unwrap();
            let _ = service
                .exec_query("CREATE TABLE s.Bytes (id int, data bytea)")
                .await
                .unwrap();
            let _ = service
                .exec_query(
                    "INSERT INTO s.Bytes(id, data) VALUES (1, '01 ff 1a'), (2, X'deADbeef'), (3, 456)",
                )
                .await
                .unwrap();

            let result = service.exec_query("SELECT * from s.Bytes").await.unwrap();
            let r = result.get_rows();
            assert_eq!(r.len(), 3);
            assert_eq!(
                r[0].values()[1],
                TableValue::Bytes(vec![0x01, 0xff, 0x1a])
            );
            assert_eq!(
                r[1].values()[1],
                TableValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
            );
            assert_eq!(
                r[2].values()[1],
                TableValue::Bytes("456".as_bytes().to_vec())
            );
        })
        .await;
    }
    #[tokio::test]
    async fn hyperloglog() {
        Config::run_test("hyperloglog", async move |services| {
            let service = services.sql_service;

            let _ = service
                .exec_query("CREATE SCHEMA IF NOT EXISTS hll")
                .await
                .unwrap();
            let _ = service
                .exec_query("CREATE TABLE hll.sketches (id int, hll varbinary)")
                .await
                .unwrap();

            let sparse = "X'020C0200C02FF58941D5F0C6'";
            let dense = "X'030C004020000001000000000000000000000000000000000000050020000001030100000410000000004102100000000000000051000020000020003220000003102000000000001200042000000001000200000002000000100000030040000000010040003010000000000100002000000000000000000031000020000000000000000000100000200302000000000000000000001002000000000002204000000001000001000200400000000000001000020031100000000080000000002003000000100000000100110000000000000000000010000000000000000000000020000001320205000100000612000000000004100020100000000000000000001000000002200000100000001000001020000000000020000000000000001000010300060000010000000000070100003000000000000020000000000001000010000104000000000000000000101000100000001401000000000000000000000000000100010000000000000000000000000400020000000002002300010000000000040000041000200005100000000000001000000000100000203010000000000000000000000000001006000100000000000000300100001000100254200000000000101100040000000020000010000050000000501000000000101020000000010000000003000000000200000102100000000204007000000200010000033000000000061000000000000000000000000000000000100001000001000000013000000003000000000002000000000000010001000000000000000000020010000020000000100001000000000000001000103000000000000000000020020000001000000000100001000000000000000020220200200000001001000010100000000200000000000001000002000000011000000000101200000000000000000000000000000000000000100130000000000000000000100000120000300040000000002000000000000000000000100000000070000100000000301000000401200002020000000000601030001510000000000000110100000000000000000050000000010000100000000000000000100022000100000101054010001000000000000001000001000000002000000000100000000000021000001000002000000000100000000000000000000951000000100000000000000000000000000102000200000000000000010000010000000000100002000000000000000000010000000000000010000000010000000102010000000010520100000021010100000030000000000000000100000001000000022000330051000000100000000000040003020000010000020000100000013000000102020000000050000000020010000000000000000101200C000100000001200400000000010000001000000000100010000000001000001000000100000000010000000004000000002000013102000100000000000000000000000600000010000000000000020000000000001000000000030000000000000020000000001000001000000000010000003002000003000200070001001003030010000000003000000000000020000006000000000000000011000000010000200000000000500000000000000020500000000003000000000000000004000030000100000000103000001000000000000200002004200000020000000030000000000000000000000002000100000000000000002000000000000000010020101000000005250000010000000000023010000001000000000000500002001000123100030011000020001310600000000000021000023000003000000000000000001000000000000220200000000004040000020201000000010201000000000020000400010000050000000000000000000000010000020000000000000000000000000000000000102000010000000000000000000000002010000200200000000000000000000000000100000000000000000200400000000010000000000000000000000000000000010000200300000000000100110000000000000000000000000010000030000001000000000010000010200013000000000000200000001000001200010000000010000000000001000000000000100000000410000040000001000100010000100000002001010000000000000000001000000000000010000000000000000000000002000000000001100001000000001010000000000000002200000000004000000000000100010000000000600000000100300000000000000000000010000003000000000000000000310000010100006000010001000000000000001010101000100000000000000000000000000000201000000000000000700010000030000000000000021000000000000000001020000000030000100001000000000000000000000004010100000000000000000000004000000040100000040100100001000000000300000100000000010010000300000200000000000001302000000000000000000100100000400030000001001000100100002300000004030000002010000220100000000000002000000010010000000003010500000000300000000005020102000200000000000000020100000000000000000000000011000000023000000000010000101000000000000010020040200040000020000004000020000000001000000000100000200000010000000000030100010001000000100000000000600400000000002000000000000132000000900010000000030021400000000004100006000304000000000000010000106000001300020000'";

            service.exec_query(&format!("INSERT INTO hll.sketches (id, hll) VALUES (1, {s}), (2, {d}), (3, {s}), (4, {d})", s = sparse, d = dense)).await.unwrap();

            //  Check cardinality.
            let result = service
                .exec_query("SELECT id, cardinality(hll) as cnt from hll.sketches WHERE id < 3 ORDER BY 1")
                .await
                .unwrap();
            assert_eq!(
                result.get_rows().iter().map(|r| r.values().clone()).collect_vec(),
                vec![vec![TableValue::Int(1), TableValue::Int(2)],
                     vec![TableValue::Int(2), TableValue::Int(655)]]);
            // Check merge and cardinality.
            let result = service
                .exec_query("SELECT cardinality(merge(hll)) from hll.sketches WHERE id < 3")
                .await
                .unwrap();
            assert_eq!(
                result.get_rows().iter().map(|r| r.values().clone()).collect_vec(),
                vec![vec![TableValue::Int(657)]]);

            // Now merge all 4 HLLs, results should stay the same.
            let result = service
                .exec_query("SELECT cardinality(merge(hll)) from hll.sketches")
                .await
                .unwrap();
            assert_eq!(
                result.get_rows().iter().map(|r| r.values().clone()).collect_vec(),
                vec![vec![TableValue::Int(657)]]);

            // TODO: add format checks on insert and test invalid inputs.
        })
        .await;
    }

    #[tokio::test]
    async fn hyperloglog_inserts() {
        Config::run_test("hyperloglog_inserts", async move |services| {
            let service = services.sql_service;

            let _ = service
                .exec_query("CREATE SCHEMA IF NOT EXISTS hll")
                .await
                .unwrap();
            let _ = service
                .exec_query("CREATE TABLE hll.sketches (id int, hll hyperloglog)")
                .await
                .unwrap();

            service.exec_query("INSERT INTO hll.sketches(id, hll) VALUES (0, X'')")
                .await.expect_err("should not allow invalid HLL");
            service.exec_query("INSERT INTO hll.sketches(id, hll) VALUES (0, X'020C0200C02FF58941D5F0C6')")
                .await.expect("should allow valid HLL");
            service.exec_query("INSERT INTO hll.sketches(id, hll) VALUES (0, X'020C0200C02FF58941D5F0C6123')")
                .await.expect_err("should not allow invalid HLL (with extra bytes)");
        }).await;
    }


    #[tokio::test]
    async fn compaction() {
        Config::test("compaction").update_config(|mut config| {
            config.partition_split_threshold = 5;
            config.compaction_chunks_count_threshold = 0;
            config
        }).start_test(async move |services| {
            let service = services.sql_service;

            service.exec_query("CREATE SCHEMA foo").await.unwrap();

            service.exec_query("CREATE TABLE foo.table (t int)").await.unwrap();

            let listener = services.cluster.job_result_listener();

            service.exec_query(
                "INSERT INTO foo.table (t) VALUES (NULL), (1), (3), (5), (10), (20), (25), (25), (25), (25), (25)"
            ).await.unwrap();

            service.exec_query(
                "INSERT INTO foo.table (t) VALUES (NULL), (NULL), (NULL), (2), (4), (5), (27), (28), (29)"
            ).await.unwrap();

            listener.wait_for_job_results(vec![
                (RowKey::Table(TableId::Partitions, 1), JobType::PartitionCompaction),
                (RowKey::Table(TableId::Partitions, 2), JobType::PartitionCompaction),
                (RowKey::Table(TableId::Partitions, 3), JobType::PartitionCompaction),
                (RowKey::Table(TableId::Partitions, 1), JobType::Repartition),
                (RowKey::Table(TableId::Partitions, 2), JobType::Repartition),
                (RowKey::Table(TableId::Partitions, 3), JobType::Repartition),
            ]).await.unwrap();

            let partitions = services.meta_store.get_active_partitions_by_index_id(1).await.unwrap();

            assert_eq!(partitions.len(), 4);
            let p_1 = partitions.iter().find(|r| r.get_id() == 5).unwrap();
            let p_2 = partitions.iter().find(|r| r.get_id() == 6).unwrap();
            let p_3 = partitions.iter().find(|r| r.get_id() == 7).unwrap();
            let p_4 = partitions.iter().find(|r| r.get_id() == 8).unwrap();
            let new_partitions = vec![p_1, p_2, p_3, p_4];
            println!("{:?}", new_partitions);
            let intervals_set = new_partitions.into_iter()
                .map(|p| (p.get_row().get_min_val().clone(), p.get_row().get_max_val().clone()))
                .collect::<Vec<_>>();
            assert_eq!(intervals_set, vec![
                (None, Some(Row::new(vec![TableValue::Int(2)]))),
                (Some(Row::new(vec![TableValue::Int(2)])), Some(Row::new(vec![TableValue::Int(10)]))),
                (Some(Row::new(vec![TableValue::Int(10)])), Some(Row::new(vec![TableValue::Int(27)]))),
                (Some(Row::new(vec![TableValue::Int(27)])), None),
            ].into_iter().collect::<Vec<_>>());

            let result = service.exec_query("SELECT count(*) from foo.table").await.unwrap();

            assert_eq!(result.get_rows()[0], Row::new(vec![TableValue::Int(20)]));
        }).await;
    }
}

impl SqlServiceImpl {
    fn handle_workbench_queries(q: &str) -> Option<DataFrame> {
        if q == "SHOW SESSION VARIABLES LIKE 'lower_case_table_names'" {
            return Some(DataFrame::new(
                vec![
                    Column::new("Variable_name".to_string(), ColumnType::String, 0),
                    Column::new("Value".to_string(), ColumnType::String, 1),
                ],
                vec![Row::new(vec![
                    TableValue::String("lower_case_table_names".to_string()),
                    TableValue::String("2".to_string()),
                ])],
            ));
        }
        if q == "SHOW SESSION VARIABLES LIKE 'sql_mode'" {
            return Some(DataFrame::new(
                vec![
                    Column::new("Variable_name".to_string(), ColumnType::String, 0),
                    Column::new("Value".to_string(), ColumnType::String, 1),
                ],
                vec![Row::new(vec![
                    TableValue::String("sql_mode".to_string()),
                    TableValue::String("TRADITIONAL".to_string()),
                ])],
            ));
        }
        if q.to_lowercase() == "select current_user()" {
            return Some(DataFrame::new(
                vec![Column::new("user".to_string(), ColumnType::String, 0)],
                vec![Row::new(vec![TableValue::String("root".to_string())])],
            ));
        }
        if q.to_lowercase() == "select connection_id()" {
            // TODO
            return Some(DataFrame::new(
                vec![Column::new(
                    "connection_id".to_string(),
                    ColumnType::String,
                    0,
                )],
                vec![Row::new(vec![TableValue::String("1".to_string())])],
            ));
        }
        if q.to_lowercase() == "select connection_id() as connectionid" {
            // TODO
            return Some(DataFrame::new(
                vec![Column::new(
                    "connectionId".to_string(),
                    ColumnType::String,
                    0,
                )],
                vec![Row::new(vec![TableValue::String("1".to_string())])],
            ));
        }
        if q.to_lowercase() == "set character set utf8" {
            return Some(DataFrame::new(vec![], vec![]));
        }
        if q.to_lowercase() == "set names utf8" {
            return Some(DataFrame::new(vec![], vec![]));
        }
        if q.to_lowercase() == "show character set where charset = 'utf8mb4'" {
            return Some(DataFrame::new(vec![], vec![]));
        }
        None
    }
}
