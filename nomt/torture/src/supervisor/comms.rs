use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::bail;
use futures::SinkExt;
use tokio::{
    io::{BufReader, BufWriter},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixStream,
    },
    sync::{oneshot, Mutex},
    time::timeout,
};
use tokio_serde::{formats::SymmetricalBincode, SymmetricallyFramed};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::message::{self, Envelope, ToAgent, ToSupervisor, MAX_ENVELOPE_SIZE};

/// The type definition of a sink which is built:
///
/// - bincode serializer using [`Envelope<ToAgent>`].
/// - length-delimited codec.
/// - buf writer.
/// - unix stream (write half).
type WrStream = SymmetricallyFramed<
    FramedWrite<BufWriter<OwnedWriteHalf>, LengthDelimitedCodec>,
    Envelope<ToAgent>,
    SymmetricalBincode<Envelope<ToAgent>>,
>;
/// The type definition of a stream which is built:
///
/// - unix stream (read half).
/// - buf reader.
/// - length-delimited codec.
/// - bincode deserializer using [`Envelope<ToSupervisor>`].
type RdStream = SymmetricallyFramed<
    FramedRead<BufReader<OwnedReadHalf>, LengthDelimitedCodec>,
    Envelope<ToSupervisor>,
    SymmetricalBincode<Envelope<ToSupervisor>>,
>;

/// A means to communicate with an agent.
///
/// This is a request-response client. Cheaply cloneable.
#[derive(Clone)]
pub struct RequestResponse {
    shared: Arc<Shared>,
}

struct Shared {
    /// The next request number to be used.
    reqno: AtomicU64,
    wr_stream: Mutex<WrStream>,
    pending: Mutex<HashMap<u64, oneshot::Sender<ToSupervisor>>>,
    timeout: Duration,
}

impl RequestResponse {
    /// Sends a given request to the agent.
    ///
    /// Returns a response.
    pub async fn send_request(&self, message: ToAgent) -> anyhow::Result<ToSupervisor> {
        let reqno = self.shared.reqno.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        let mut pending = self.shared.pending.lock().await;
        pending.insert(reqno, tx);
        drop(pending);

        let mut wr_stream = self.shared.wr_stream.lock().await;
        wr_stream.send(Envelope { reqno, message }).await?;
        drop(wr_stream);

        let message = timeout(self.shared.timeout, rx).await??;

        Ok(message)
    }

    /// Requests the value of the key from the agent.
    pub async fn send_request_query(&self, key: message::Key) -> anyhow::Result<Option<Vec<u8>>> {
        match self
            .send_request(crate::message::ToAgent::Query(key))
            .await?
        {
            crate::message::ToSupervisor::QueryValue(vec) => Ok(vec),
            resp => bail!("unexpected response: {:?}", resp),
        }
    }

    /// Requests the current sync sequence number from the agent.
    pub async fn send_query_sync_seqn(&self) -> anyhow::Result<u32> {
        match self
            .send_request(crate::message::ToAgent::QuerySyncSeqn)
            .await?
        {
            crate::message::ToSupervisor::SyncSeqn(seqn) => Ok(seqn),
            resp => bail!("unexpected response: {:?}", resp),
        }
    }
}

/// A task that handles inbound messages and dispatches them to the corresponding request listener.
///
/// Returns an error if the stream is closed, the request number is unknown, or the message is
/// malformed.
async fn handle_inbound(shared: Arc<Shared>, mut rd_stream: RdStream) -> anyhow::Result<()> {
    loop {
        let envelope = match rd_stream.try_next().await {
            Ok(None) => {
                bail!("agent unixstream read half finished");
            }
            Ok(Some(envelope)) => envelope,
            Err(e) => bail!(e),
        };
        let Envelope { reqno, message } = envelope;
        let mut pending = shared.pending.lock().await;
        if let Some(tx) = pending.remove(&reqno) {
            // We don't care if the receiver is gone.
            let _ = tx.send(message);
        } else {
            bail!("unknown reqno")
        }
    }
}

/// Runs the communication handling logic.
///
/// The returned future should be polled. If the future resolves to an error, the connection should
/// be closed.
pub fn run(stream: UnixStream) -> (RequestResponse, impl Future<Output = anyhow::Result<()>>) {
    let (rd, wr) = stream.into_split();

    let wr_stream = SymmetricallyFramed::new(
        FramedWrite::new(
            BufWriter::new(wr),
            LengthDelimitedCodec::builder()
                .length_field_length(8)
                .max_frame_length(MAX_ENVELOPE_SIZE)
                .new_codec(),
        ),
        SymmetricalBincode::default(),
    );
    let rd_stream = SymmetricallyFramed::new(
        FramedRead::new(
            BufReader::new(rd),
            LengthDelimitedCodec::builder()
                .length_field_length(8)
                .max_frame_length(MAX_ENVELOPE_SIZE)
                .new_codec(),
        ),
        SymmetricalBincode::default(),
    );

    let timeout = Duration::from_secs(60);

    let shared = Arc::new(Shared {
        reqno: AtomicU64::new(0),
        wr_stream: Mutex::new(wr_stream),
        pending: Mutex::new(HashMap::new()),
        timeout,
    });
    let rr = RequestResponse {
        shared: shared.clone(),
    };

    let inbound = handle_inbound(shared, rd_stream);

    (rr, inbound)
}
