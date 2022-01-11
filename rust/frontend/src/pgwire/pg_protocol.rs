use log::info;
use risingwave_common::error::Result;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::pgwire::database::Database;
use crate::pgwire::pg_message::{
    BeCommandCompleteMessage, BeMessage, BeParameterStatusMessage, FeMessage, FeQueryMessage,
    FeStartupMessage,
};
use crate::pgwire::pg_result::PgResult;

/// The state machine for each psql connection.
/// Read pg messages from tcp stream and write results back.
pub struct PgProtocol {
    /// Used for write/read message.
    stream: TcpStream,
    /// Current states of pg connection.
    state: PgProtocolState,
    /// Whether the connection is terminated.
    is_terminate: bool,
    database: Database,
}

/// States flow happened from top to down.
enum PgProtocolState {
    Startup,
    Regular,
}

impl PgProtocol {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            is_terminate: false,
            state: PgProtocolState::Startup,
            database: Database {},
        }
    }

    pub async fn process(&mut self) -> Result<bool> {
        if self.do_process().await? {
            return Ok(true);
        }

        Ok(self.is_terminate())
    }

    async fn do_process(&mut self) -> Result<bool> {
        let msg = self.read_message().await.unwrap();
        match msg {
            FeMessage::Ssl => {
                self.write_message_no_flush(&BeMessage::EncryptionResponse)
                    .await?;
            }
            FeMessage::Startup(msg) => {
                self.process_startup_msg(msg).await?;
                self.state = PgProtocolState::Regular;
            }
            FeMessage::Query(query_msg) => {
                self.process_query_msg(query_msg).await?;
            }
            FeMessage::Terminate => {
                self.process_terminate();
            }
        }
        self.stream.flush().await?;
        Ok(false)
    }

    async fn read_message(&mut self) -> Result<FeMessage> {
        match self.state {
            PgProtocolState::Startup => FeStartupMessage::read(&mut self.stream).await,
            PgProtocolState::Regular => FeMessage::read(&mut self.stream).await,
        }
    }

    async fn process_startup_msg(&mut self, _msg: FeStartupMessage) -> Result<()> {
        self.write_message_no_flush(&BeMessage::AuthenticationOk)
            .await?;
        self.write_message_no_flush(&BeMessage::ParameterStatus(
            BeParameterStatusMessage::Encoding("utf8"),
        ))
        .await?;
        self.write_message_no_flush(&BeMessage::ParameterStatus(
            BeParameterStatusMessage::StandardConformingString("on"),
        ))
        .await?;
        self.write_message_no_flush(&BeMessage::ReadyForQuery)
            .await?;
        Ok(())
    }

    fn process_terminate(&mut self) {
        self.is_terminate = true;
    }

    async fn process_query_msg(&mut self, query: FeQueryMessage) -> Result<()> {
        info!("receive query: {}", query.get_sql());
        // execute query
        let res = self.database.run_statement(query.get_sql());
        if res.is_query() {
            self.process_query_with_results(res).await?;
        } else {
            self.write_message_no_flush(&BeMessage::CommandComplete(BeCommandCompleteMessage {
                stmt_type: res.get_stmt_type(),
                rows_cnt: res.get_effected_rows_cnt(),
            }))
            .await?;
        }
        self.write_message_no_flush(&BeMessage::ReadyForQuery)
            .await?;
        Ok(())
    }

    async fn process_query_with_results(&mut self, res: PgResult) -> Result<()> {
        self.write_message(&BeMessage::RowDescription(&res.get_row_desc()))
            .await?;

        let mut rows_cnt = 0;
        let iter = res.iter();
        for val in iter {
            self.write_message(&BeMessage::DataRow(val)).await?;
            rows_cnt += 1;
        }
        self.write_message_no_flush(&BeMessage::CommandComplete(BeCommandCompleteMessage {
            stmt_type: res.get_stmt_type(),
            rows_cnt,
        }))
        .await?;
        Ok(())
    }

    fn is_terminate(&self) -> bool {
        self.is_terminate
    }

    pub async fn write_message_no_flush(&mut self, message: &BeMessage<'_>) -> Result<()> {
        BeMessage::write(&mut self.stream, message).await
    }

    pub async fn write_message(&mut self, message: &BeMessage<'_>) -> Result<()> {
        self.write_message_no_flush(message).await?;
        self.stream.flush().await?;
        Ok(())
    }
}
