use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{bail, ensure, Context};
use borderless_core::file_transfer::{
    FileChunk, FileManifestEntry, FileTransferManifest, FileTransferState,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{tcp::OwnedReadHalf, tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, Duration},
};
use uuid::Uuid;
use walkdir::WalkDir;

const CHUNK_SIZE: usize = 1024 * 1024;
const MAX_BULK_FRAME_LEN: usize = CHUNK_SIZE + 64 * 1024;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub enum BulkTransferEvent {
    Offered(FileTransferManifest),
    Progress {
        transfer_id: Uuid,
        bytes_done: u64,
        bytes_total: u64,
        current_file: String,
    },
    Sent {
        transfer_id: Uuid,
    },
    Completed {
        transfer_id: Uuid,
        cache_paths: Vec<String>,
    },
    Cancelled(Uuid),
    Failed {
        transfer_id: Uuid,
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BulkTransferCommand {
    SendManifest(FileTransferManifest),
    SendFiles {
        manifest: FileTransferManifest,
        source_paths: Vec<String>,
    },
    Cancel(Uuid),
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
enum BulkWireMessage {
    Manifest(FileTransferManifest),
    FileHeader {
        transfer_id: Uuid,
        relative_path: String,
        size_bytes: u64,
        blake3_hex: String,
    },
    Chunk(FileChunk),
    FileComplete {
        transfer_id: Uuid,
        relative_path: String,
        blake3_hex: String,
    },
    TransferComplete {
        transfer_id: Uuid,
    },
    Cancel {
        transfer_id: Uuid,
    },
}

enum WriterCommand {
    Frame(BulkWireMessage),
    Files {
        manifest: FileTransferManifest,
        source_paths: Vec<String>,
    },
    Stop,
}

#[derive(Default)]
struct ReceiveBook {
    transfers: HashMap<Uuid, ReceiveTransfer>,
}

struct ReceiveTransfer {
    manifest: FileTransferManifest,
    state: FileTransferState,
    bytes_done: u64,
    cache_paths: Vec<String>,
    resolved_roots: HashMap<String, PathBuf>,
    completed_files: HashSet<String>,
}

struct ActiveReceiveFile {
    transfer_id: Uuid,
    relative_path: String,
    final_path: PathBuf,
    temp_path: PathBuf,
    expected_size: u64,
    expected_hash: String,
    written: u64,
    hasher: blake3::Hasher,
    file: tokio::fs::File,
}

pub fn rename_conflict(name: &str, index: u32) -> String {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    let suffix = format!(" (Borderless {index})");

    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if !extension.is_empty() => format!("{stem}{suffix}.{extension}"),
        _ => format!("{stem}{suffix}"),
    }
}

pub fn manifest_from_source_paths(
    transfer_id: Uuid,
    source_paths: &[String],
) -> anyhow::Result<FileTransferManifest> {
    let entries = expand_manifest_entries(source_paths)?;
    let root_name = transfer_root_name(source_paths);
    let total_bytes = entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .fold(0u64, |total, entry| total.saturating_add(entry.size_bytes));

    Ok(FileTransferManifest {
        transfer_id,
        root_name,
        files: entries,
        total_bytes,
    })
}

pub async fn run_bulk_transfer_server(
    listen_host: String,
    port: u16,
    incoming_cache_dir: String,
    events: mpsc::UnboundedSender<BulkTransferEvent>,
    mut commands: mpsc::UnboundedReceiver<BulkTransferCommand>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(format!("{listen_host}:{port}")).await?;
    let mut pending = VecDeque::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if run_connected_session(
                    stream,
                    incoming_cache_dir.clone(),
                    events.clone(),
                    &mut commands,
                    &mut pending,
                )
                .await?
                {
                    return Ok(());
                }
            }
            command = commands.recv() => {
                match command {
                    Some(BulkTransferCommand::Stop) | None => return Ok(()),
                    Some(command) => pending.push_back(command),
                }
            }
        }
    }
}

pub async fn run_bulk_transfer_client(
    host: String,
    port: u16,
    incoming_cache_dir: String,
    events: mpsc::UnboundedSender<BulkTransferEvent>,
    mut commands: mpsc::UnboundedReceiver<BulkTransferCommand>,
) -> anyhow::Result<()> {
    let peer = format!("{host}:{port}");
    let mut pending = VecDeque::new();

    'reconnect: loop {
        let connect = TcpStream::connect(&peer);
        tokio::pin!(connect);

        let stream = loop {
            tokio::select! {
                connected = &mut connect => {
                    match connected {
                        Ok(stream) => break stream,
                        Err(_) => {
                            if wait_before_retry(&mut commands, &mut pending).await {
                                return Ok(());
                            }
                            continue 'reconnect;
                        }
                    }
                }
                command = commands.recv() => {
                    match command {
                        Some(BulkTransferCommand::Stop) | None => return Ok(()),
                        Some(command) => pending.push_back(command),
                    }
                }
            }
        };

        if run_connected_session(
            stream,
            incoming_cache_dir.clone(),
            events.clone(),
            &mut commands,
            &mut pending,
        )
        .await?
        {
            return Ok(());
        }
    }
}

async fn wait_before_retry(
    commands: &mut mpsc::UnboundedReceiver<BulkTransferCommand>,
    pending: &mut VecDeque<BulkTransferCommand>,
) -> bool {
    let delay = sleep(CONNECT_RETRY_DELAY);
    tokio::pin!(delay);

    loop {
        tokio::select! {
            _ = &mut delay => return false,
            command = commands.recv() => {
                match command {
                    Some(BulkTransferCommand::Stop) | None => return true,
                    Some(command) => pending.push_back(command),
                }
            }
        }
    }
}

async fn run_connected_session(
    stream: TcpStream,
    incoming_cache_dir: String,
    events: mpsc::UnboundedSender<BulkTransferEvent>,
    commands: &mut mpsc::UnboundedReceiver<BulkTransferCommand>,
    pending: &mut VecDeque<BulkTransferCommand>,
) -> anyhow::Result<bool> {
    stream.set_nodelay(true).context("enable TCP_NODELAY")?;
    let (reader, writer) = stream.into_split();
    let cancelled = Arc::new(Mutex::new(HashSet::new()));
    let (writer_tx, writer_rx) = mpsc::unbounded_channel();
    let writer_task = spawn_writer(writer, writer_rx, events.clone(), Arc::clone(&cancelled));
    let reader_task = tokio::spawn(read_incoming(
        reader,
        incoming_cache_dir,
        events.clone(),
        Arc::clone(&cancelled),
    ));

    while let Some(command) = pending.pop_front() {
        if handle_session_command(command, &writer_tx, &events, &cancelled)? {
            stop_connected_tasks(writer_tx, writer_task, reader_task).await;
            return Ok(true);
        }
    }

    tokio::pin!(reader_task);
    tokio::pin!(writer_task);

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(command) => {
                        if handle_session_command(command, &writer_tx, &events, &cancelled)? {
                            writer_task.abort();
                            reader_task.abort();
                            let _ = (&mut writer_task).await;
                            let _ = (&mut reader_task).await;
                            return Ok(true);
                        }
                    }
                    None => {
                        let _ = writer_tx.send(WriterCommand::Stop);
                        writer_task.abort();
                        reader_task.abort();
                        let _ = (&mut writer_task).await;
                        let _ = (&mut reader_task).await;
                        return Ok(true);
                    }
                }
            }
            result = &mut reader_task => {
                let _ = result;
                let _ = writer_tx.send(WriterCommand::Stop);
                let _ = (&mut writer_task).await;
                return Ok(false);
            }
            result = &mut writer_task => {
                let _ = result;
                reader_task.abort();
                let _ = (&mut reader_task).await;
                return Ok(false);
            }
        }
    }
}

fn handle_session_command(
    command: BulkTransferCommand,
    writer_tx: &mpsc::UnboundedSender<WriterCommand>,
    events: &mpsc::UnboundedSender<BulkTransferEvent>,
    cancelled: &Arc<Mutex<HashSet<Uuid>>>,
) -> anyhow::Result<bool> {
    match command {
        BulkTransferCommand::SendManifest(manifest) => {
            writer_tx.send(WriterCommand::Frame(BulkWireMessage::Manifest(manifest)))?;
            Ok(false)
        }
        BulkTransferCommand::SendFiles {
            manifest,
            source_paths,
        } => {
            writer_tx.send(WriterCommand::Files {
                manifest,
                source_paths,
            })?;
            Ok(false)
        }
        BulkTransferCommand::Cancel(transfer_id) => {
            mark_cancelled(cancelled, transfer_id);
            let _ = events.send(BulkTransferEvent::Cancelled(transfer_id));
            writer_tx.send(WriterCommand::Frame(BulkWireMessage::Cancel {
                transfer_id,
            }))?;
            Ok(false)
        }
        BulkTransferCommand::Stop => {
            let _ = writer_tx.send(WriterCommand::Stop);
            Ok(true)
        }
    }
}

async fn stop_connected_tasks(
    writer_tx: mpsc::UnboundedSender<WriterCommand>,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<anyhow::Result<()>>,
) {
    let _ = writer_tx.send(WriterCommand::Stop);
    let _ = writer_task.await;
    reader_task.abort();
    let _ = reader_task.await;
}

fn spawn_writer(
    mut writer: OwnedWriteHalf,
    mut writer_rx: mpsc::UnboundedReceiver<WriterCommand>,
    events: mpsc::UnboundedSender<BulkTransferEvent>,
    cancelled: Arc<Mutex<HashSet<Uuid>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(command) = writer_rx.recv().await {
            let result = match command {
                WriterCommand::Frame(message) => send_wire(&mut writer, &message).await,
                WriterCommand::Files {
                    manifest,
                    source_paths,
                } => {
                    let transfer_id = manifest.transfer_id;
                    let result =
                        send_files(&mut writer, manifest, source_paths, &events, &cancelled).await;
                    if let Err(error) = &result {
                        let _ = events.send(BulkTransferEvent::Failed {
                            transfer_id,
                            error: error.to_string(),
                        });
                    }
                    result
                }
                WriterCommand::Stop => break,
            };

            if let Err(error) = result {
                tracing::warn!("bulk transfer writer failed: {error}");
                break;
            }
        }
    })
}

async fn send_files<W>(
    writer: &mut W,
    manifest: FileTransferManifest,
    source_paths: Vec<String>,
    events: &mpsc::UnboundedSender<BulkTransferEvent>,
    cancelled: &Arc<Mutex<HashSet<Uuid>>>,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let transfer_id = manifest.transfer_id;
    let files = expand_sources(&source_paths)?;
    send_wire(writer, &BulkWireMessage::Manifest(manifest.clone())).await?;
    let mut bytes_done = 0_u64;

    for source in files {
        if is_cancelled(cancelled, transfer_id) {
            send_wire(writer, &BulkWireMessage::Cancel { transfer_id }).await?;
            let _ = events.send(BulkTransferEvent::Cancelled(transfer_id));
            return Ok(());
        }

        let size_bytes = source.size_bytes;
        let source_path = source.path.clone();
        let cancelled_for_hash = Arc::clone(cancelled);
        let hash_result = tokio::task::spawn_blocking(move || {
            hash_file_hex(&source_path, transfer_id, &cancelled_for_hash)
        })
        .await
        .context("hash computation task panicked")?;
        let final_hash = match hash_result {
            Ok(hash) => hash,
            Err(_) if is_cancelled(cancelled, transfer_id) => {
                send_wire(writer, &BulkWireMessage::Cancel { transfer_id }).await?;
                let _ = events.send(BulkTransferEvent::Cancelled(transfer_id));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        send_wire(
            writer,
            &BulkWireMessage::FileHeader {
                transfer_id,
                relative_path: source.relative_path.clone(),
                size_bytes,
                blake3_hex: final_hash.clone(),
            },
        )
        .await?;

        let mut file = tokio::fs::File::open(&source.path)
            .await
            .with_context(|| format!("open source file {}", source.path.display()))?;
        let mut offset = 0_u64;
        let mut buffer = vec![0; CHUNK_SIZE];

        loop {
            if is_cancelled(cancelled, transfer_id) {
                send_wire(writer, &BulkWireMessage::Cancel { transfer_id }).await?;
                let _ = events.send(BulkTransferEvent::Cancelled(transfer_id));
                return Ok(());
            }

            let read = file
                .read(&mut buffer)
                .await
                .with_context(|| format!("read source file {}", source.path.display()))?;
            if read == 0 {
                break;
            }

            let bytes = buffer[..read].to_vec();
            let chunk_hash = blake3::hash(&bytes).to_hex().to_string();
            send_wire(
                writer,
                &BulkWireMessage::Chunk(FileChunk {
                    transfer_id,
                    relative_path: source.relative_path.clone(),
                    offset,
                    bytes,
                    blake3_hex: chunk_hash,
                }),
            )
            .await?;
            offset = offset.saturating_add(read as u64);
            bytes_done = bytes_done.saturating_add(read as u64);
            let _ = events.send(BulkTransferEvent::Progress {
                transfer_id,
                bytes_done,
                bytes_total: manifest.total_bytes,
                current_file: source.relative_path.clone(),
            });
        }

        send_wire(
            writer,
            &BulkWireMessage::FileComplete {
                transfer_id,
                relative_path: source.relative_path,
                blake3_hex: final_hash,
            },
        )
        .await?;
    }

    send_wire(writer, &BulkWireMessage::TransferComplete { transfer_id }).await?;
    let _ = events.send(BulkTransferEvent::Sent { transfer_id });
    Ok(())
}

async fn read_incoming(
    mut reader: OwnedReadHalf,
    incoming_cache_dir: String,
    events: mpsc::UnboundedSender<BulkTransferEvent>,
    cancelled: Arc<Mutex<HashSet<Uuid>>>,
) -> anyhow::Result<()> {
    let cache_dir = PathBuf::from(incoming_cache_dir);
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .with_context(|| format!("create incoming cache {}", cache_dir.display()))?;
    let mut book = ReceiveBook::default();
    let mut active_file: Option<ActiveReceiveFile> = None;

    loop {
        let message = read_wire(&mut reader).await?;
        match message {
            BulkWireMessage::Manifest(manifest) => {
                let transfer_id = manifest.transfer_id;
                if let Err(error) = handle_manifest(&cache_dir, &events, &mut book, manifest).await
                {
                    emit_bulk_failed(&events, transfer_id, &error);
                    return Err(error);
                }
            }
            BulkWireMessage::FileHeader {
                transfer_id,
                relative_path,
                size_bytes,
                blake3_hex,
            } => {
                active_file = match prepare_receive_file(
                    &book,
                    &cancelled,
                    transfer_id,
                    relative_path,
                    size_bytes,
                    blake3_hex,
                )
                .await
                {
                    Ok(file) => file,
                    Err(error) => {
                        emit_bulk_failed(&events, transfer_id, &error);
                        return Err(error);
                    }
                };
            }
            BulkWireMessage::Chunk(chunk) => {
                if is_cancelled(&cancelled, chunk.transfer_id) {
                    continue;
                }
                if let Some(file) = active_file.as_mut() {
                    let transfer_id = chunk.transfer_id;
                    if let Err(error) = receive_chunk(&events, &mut book, file, chunk).await {
                        emit_bulk_failed(&events, transfer_id, &error);
                        return Err(error);
                    }
                }
            }
            BulkWireMessage::FileComplete {
                transfer_id,
                relative_path,
                blake3_hex,
            } => {
                if is_cancelled(&cancelled, transfer_id) {
                    active_file = None;
                    continue;
                }
                if let Some(file) = active_file.take() {
                    if let Err(error) = complete_receive_file(
                        &events,
                        &mut book,
                        file,
                        transfer_id,
                        &relative_path,
                        &blake3_hex,
                    )
                    .await
                    {
                        emit_bulk_failed(&events, transfer_id, &error);
                        return Err(error);
                    }
                }
            }
            BulkWireMessage::TransferComplete { transfer_id } => {
                maybe_emit_completed(&events, &mut book, transfer_id);
            }
            BulkWireMessage::Cancel { transfer_id } => {
                mark_cancelled(&cancelled, transfer_id);
                active_file = None;
                if let Some(transfer) = book.transfers.get_mut(&transfer_id) {
                    transfer.state = FileTransferState::Cancelled;
                }
                let _ = events.send(BulkTransferEvent::Cancelled(transfer_id));
            }
        }
    }
}

async fn handle_manifest(
    cache_dir: &Path,
    events: &mpsc::UnboundedSender<BulkTransferEvent>,
    book: &mut ReceiveBook,
    manifest: FileTransferManifest,
) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(cache_dir)
        .await
        .with_context(|| format!("create cache directory {}", cache_dir.display()))?;
    let resolved_roots = resolve_manifest_roots(cache_dir, &manifest)?;
    let cache_paths = top_level_cache_paths(&manifest, &resolved_roots)?;

    book.transfers
        .entry(manifest.transfer_id)
        .and_modify(|transfer| {
            transfer.manifest = manifest.clone();
            transfer.resolved_roots = resolved_roots.clone();
            transfer.cache_paths = cache_paths.clone();
            if transfer.state != FileTransferState::Completed {
                transfer.state = FileTransferState::Offered;
            }
        })
        .or_insert_with(|| ReceiveTransfer {
            manifest: manifest.clone(),
            state: FileTransferState::Offered,
            bytes_done: 0,
            cache_paths: cache_paths.clone(),
            resolved_roots: resolved_roots.clone(),
            completed_files: HashSet::new(),
        });
    let transfer = book
        .transfers
        .get(&manifest.transfer_id)
        .context("manifest transfer should be present")?;
    for entry in &manifest.files {
        if entry.is_dir {
            tokio::fs::create_dir_all(resolved_cache_path(transfer, &entry.relative_path)?)
                .await
                .with_context(|| format!("create directory for {}", entry.relative_path))?;
        }
    }
    let _ = events.send(BulkTransferEvent::Offered(manifest));
    Ok(())
}

async fn prepare_receive_file(
    book: &ReceiveBook,
    cancelled: &Arc<Mutex<HashSet<Uuid>>>,
    transfer_id: Uuid,
    relative_path: String,
    size_bytes: u64,
    blake3_hex: String,
) -> anyhow::Result<Option<ActiveReceiveFile>> {
    if is_cancelled(cancelled, transfer_id)
        || book
            .transfers
            .get(&transfer_id)
            .is_some_and(|transfer| transfer.completed_files.contains(&relative_path))
    {
        return Ok(None);
    }

    let transfer = book
        .transfers
        .get(&transfer_id)
        .with_context(|| format!("transfer manifest not found for {transfer_id}"))?;
    let final_path = resolved_cache_path(transfer, &relative_path)?;
    if let Some(parent) = final_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create directory for {}", final_path.display()))?;
    }
    let temp_path = temp_part_path(&final_path);
    if temp_path.exists() {
        tokio::fs::remove_file(&temp_path)
            .await
            .with_context(|| format!("remove stale temp file {}", temp_path.display()))?;
    }
    let file = tokio::fs::File::create(&temp_path)
        .await
        .with_context(|| format!("create temporary file {}", temp_path.display()))?;

    Ok(Some(ActiveReceiveFile {
        transfer_id,
        relative_path,
        final_path,
        temp_path,
        expected_size: size_bytes,
        expected_hash: blake3_hex,
        written: 0,
        hasher: blake3::Hasher::new(),
        file,
    }))
}

async fn receive_chunk(
    events: &mpsc::UnboundedSender<BulkTransferEvent>,
    book: &mut ReceiveBook,
    active: &mut ActiveReceiveFile,
    chunk: FileChunk,
) -> anyhow::Result<()> {
    ensure!(
        chunk.transfer_id == active.transfer_id,
        "chunk transfer id mismatch"
    );
    ensure!(
        chunk.relative_path == active.relative_path,
        "chunk relative path mismatch"
    );
    ensure!(chunk.offset == active.written, "chunk offset mismatch");
    ensure!(
        blake3::hash(&chunk.bytes).to_hex().to_string() == chunk.blake3_hex,
        "chunk checksum mismatch"
    );

    active.file.write_all(&chunk.bytes).await?;
    active.hasher.update(&chunk.bytes);
    active.written = active.written.saturating_add(chunk.bytes.len() as u64);

    if let Some(transfer) = book.transfers.get_mut(&active.transfer_id) {
        transfer.state = FileTransferState::Transferring;
        transfer.bytes_done = transfer.bytes_done.saturating_add(chunk.bytes.len() as u64);
        let _ = events.send(BulkTransferEvent::Progress {
            transfer_id: active.transfer_id,
            bytes_done: transfer.bytes_done,
            bytes_total: transfer.manifest.total_bytes,
            current_file: active.relative_path.clone(),
        });
    }

    Ok(())
}

async fn complete_receive_file(
    events: &mpsc::UnboundedSender<BulkTransferEvent>,
    book: &mut ReceiveBook,
    mut active: ActiveReceiveFile,
    transfer_id: Uuid,
    relative_path: &str,
    blake3_hex: &str,
) -> anyhow::Result<()> {
    ensure!(
        transfer_id == active.transfer_id,
        "file transfer id mismatch"
    );
    ensure!(
        relative_path == active.relative_path,
        "file relative path mismatch"
    );
    ensure!(active.written == active.expected_size, "file size mismatch");

    active.file.flush().await?;
    active.file.sync_all().await?;
    drop(active.file);
    let actual_hash = active.hasher.finalize().to_hex().to_string();
    ensure!(
        actual_hash == active.expected_hash,
        "file checksum mismatch"
    );
    ensure!(actual_hash == blake3_hex, "file complete checksum mismatch");
    ensure!(
        !active.final_path.exists(),
        "cache path appeared before final rename"
    );
    tokio::fs::rename(&active.temp_path, &active.final_path)
        .await
        .with_context(|| format!("rename {}", active.final_path.display()))?;

    if let Some(transfer) = book.transfers.get_mut(&transfer_id) {
        transfer.completed_files.insert(relative_path.to_string());
        maybe_emit_completed(events, book, transfer_id);
    }

    Ok(())
}

fn maybe_emit_completed(
    events: &mpsc::UnboundedSender<BulkTransferEvent>,
    book: &mut ReceiveBook,
    transfer_id: Uuid,
) {
    let Some(transfer) = book.transfers.get_mut(&transfer_id) else {
        return;
    };
    let expected_files = transfer
        .manifest
        .files
        .iter()
        .filter(|entry| !entry.is_dir)
        .count();

    if transfer.completed_files.len() == expected_files
        && transfer.state != FileTransferState::Completed
    {
        transfer.state = FileTransferState::Completed;
        let _ = events.send(BulkTransferEvent::Completed {
            transfer_id,
            cache_paths: transfer.cache_paths.clone(),
        });
    }
}

async fn send_wire<W>(writer: &mut W, message: &BulkWireMessage) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = bincode::serialize(message)?;
    ensure!(
        payload.len() <= MAX_BULK_FRAME_LEN,
        "bulk frame too large: max {}, got {}",
        MAX_BULK_FRAME_LEN,
        payload.len()
    );
    let len = u32::try_from(payload.len()).context("bulk frame length does not fit in u32")?;
    writer.write_u32(len).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_wire<R>(reader: &mut R) -> anyhow::Result<BulkWireMessage>
where
    R: AsyncRead + Unpin,
{
    let len = reader.read_u32().await? as usize;
    ensure!(
        len <= MAX_BULK_FRAME_LEN,
        "incoming bulk frame too large: max {}, got {}",
        MAX_BULK_FRAME_LEN,
        len
    );
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload).await?;
    Ok(bincode::deserialize(&payload)?)
}

struct SourceFile {
    relative_path: String,
    path: PathBuf,
    size_bytes: u64,
}

struct SourceRoot {
    path: PathBuf,
    relative_root: String,
    is_dir: bool,
    size_bytes: u64,
}

fn source_roots(source_paths: &[String]) -> anyhow::Result<Vec<SourceRoot>> {
    let mut roots = Vec::new();
    let mut used_roots = HashSet::new();

    for source in source_paths {
        let path = PathBuf::from(source);
        let metadata =
            fs::metadata(&path).with_context(|| format!("read metadata for {}", path.display()))?;
        let source_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("source path has no name: {}", path.display()))?;
        let relative_root = unique_relative_root(source_name, &mut used_roots);
        roots.push(SourceRoot {
            path,
            relative_root,
            is_dir: metadata.is_dir(),
            size_bytes: metadata.len(),
        });
    }

    Ok(roots)
}

fn expand_sources(source_paths: &[String]) -> anyhow::Result<Vec<SourceFile>> {
    let mut files = Vec::new();

    for source in source_roots(source_paths)? {
        if source.is_dir {
            for entry in WalkDir::new(&source.path)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.file_type().is_file() {
                    let metadata = entry
                        .metadata()
                        .with_context(|| format!("read metadata for {}", entry.path().display()))?;
                    let child_path = entry
                        .path()
                        .strip_prefix(&source.path)?
                        .to_string_lossy()
                        .replace('\\', "/");
                    files.push(SourceFile {
                        relative_path: format!("{}/{child_path}", source.relative_root),
                        path: entry.path().to_path_buf(),
                        size_bytes: metadata.len(),
                    });
                }
            }
        } else {
            files.push(SourceFile {
                relative_path: source.relative_root,
                path: source.path,
                size_bytes: source.size_bytes,
            });
        }
    }

    Ok(files)
}

fn expand_manifest_entries(source_paths: &[String]) -> anyhow::Result<Vec<FileManifestEntry>> {
    let mut entries = Vec::new();

    for source in source_roots(source_paths)? {
        if source.is_dir {
            entries.push(FileManifestEntry {
                relative_path: source.relative_root.clone(),
                size_bytes: 0,
                is_dir: true,
                blake3_hex: None,
            });
            for entry in WalkDir::new(&source.path)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.path() == source.path {
                    continue;
                }
                let child_path = entry
                    .path()
                    .strip_prefix(&source.path)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let relative_path = format!("{}/{child_path}", source.relative_root);
                if entry.file_type().is_dir() {
                    entries.push(FileManifestEntry {
                        relative_path,
                        size_bytes: 0,
                        is_dir: true,
                        blake3_hex: None,
                    });
                } else if entry.file_type().is_file() {
                    let metadata = entry
                        .metadata()
                        .with_context(|| format!("read metadata for {}", entry.path().display()))?;
                    entries.push(FileManifestEntry::file(relative_path, metadata.len()));
                }
            }
        } else {
            entries.push(FileManifestEntry::file(
                source.relative_root,
                source.size_bytes,
            ));
        }
    }

    Ok(entries)
}

fn unique_relative_root(source_name: &str, used_roots: &mut HashSet<String>) -> String {
    let mut candidate = source_name.to_string();
    for index in 1.. {
        if used_roots.insert(candidate.clone()) {
            return candidate;
        }
        candidate = rename_conflict(source_name, index);
    }

    unreachable!("conflict index range is unbounded")
}

fn transfer_root_name(source_paths: &[String]) -> String {
    if source_paths.len() == 1 {
        Path::new(&source_paths[0])
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("clipboard-files")
            .to_string()
    } else {
        "clipboard-files".to_string()
    }
}

fn hash_file_hex(
    path: &Path,
    transfer_id: Uuid,
    cancelled: &Arc<Mutex<HashSet<Uuid>>>,
) -> anyhow::Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; CHUNK_SIZE];

    loop {
        ensure!(
            !is_cancelled(cancelled, transfer_id),
            "transfer {transfer_id} cancelled while hashing {}",
            path.display()
        );
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

fn emit_bulk_failed(
    events: &mpsc::UnboundedSender<BulkTransferEvent>,
    transfer_id: Uuid,
    error: &anyhow::Error,
) {
    let _ = events.send(BulkTransferEvent::Failed {
        transfer_id,
        error: error.to_string(),
    });
}

fn resolve_manifest_roots(
    cache_dir: &Path,
    manifest: &FileTransferManifest,
) -> anyhow::Result<HashMap<String, PathBuf>> {
    let mut resolved = HashMap::new();
    let mut reserved = HashSet::new();

    for root in top_level_relative_paths(manifest)? {
        let target = available_top_level_path(cache_dir, &root, &reserved)?;
        reserved.insert(target.clone());
        resolved.insert(root, target);
    }

    Ok(resolved)
}

fn top_level_cache_paths(
    manifest: &FileTransferManifest,
    resolved_roots: &HashMap<String, PathBuf>,
) -> anyhow::Result<Vec<String>> {
    top_level_relative_paths(manifest)?
        .into_iter()
        .map(|root| {
            resolved_roots
                .get(&root)
                .map(|path| path.to_string_lossy().to_string())
                .with_context(|| format!("resolved root missing for {root}"))
        })
        .collect()
}

fn top_level_relative_paths(manifest: &FileTransferManifest) -> anyhow::Result<Vec<String>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    for entry in &manifest.files {
        let root = top_level_component(&entry.relative_path)?;
        if seen.insert(root.clone()) {
            roots.push(root);
        }
    }
    Ok(roots)
}

fn top_level_component(relative_path: &str) -> anyhow::Result<String> {
    let safe = safe_relative_path(relative_path)?;
    let mut components = safe.components();
    let Some(Component::Normal(root)) = components.next() else {
        bail!("relative path has no top-level component: {relative_path}");
    };
    Ok(root.to_string_lossy().to_string())
}

fn available_top_level_path(
    cache_dir: &Path,
    root: &str,
    reserved: &HashSet<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let safe = safe_relative_path(root)?;
    ensure!(
        safe.parent()
            .is_none_or(|parent| parent.as_os_str().is_empty()),
        "top-level root must not contain path separators: {root}"
    );
    let file_name = safe
        .file_name()
        .and_then(|name| name.to_str())
        .context("top-level root has no file name")?;
    let mut candidate = cache_dir.join(file_name);

    for index in 1.. {
        if !candidate.exists() && !reserved.contains(&candidate) {
            return Ok(candidate);
        }
        candidate = cache_dir.join(rename_conflict(file_name, index));
    }

    unreachable!("conflict index range is unbounded")
}

fn resolved_cache_path(transfer: &ReceiveTransfer, relative_path: &str) -> anyhow::Result<PathBuf> {
    let safe = safe_relative_path(relative_path)?;
    let mut components = safe.components();
    let Some(Component::Normal(root)) = components.next() else {
        bail!("relative path has no top-level component: {relative_path}");
    };
    let root = root.to_string_lossy().to_string();
    let mut target = transfer
        .resolved_roots
        .get(&root)
        .cloned()
        .with_context(|| format!("resolved root missing for {root}"))?;
    for component in components {
        if let Component::Normal(part) = component {
            target.push(part);
        }
    }
    Ok(target)
}

fn safe_relative_path(relative_path: &str) -> anyhow::Result<PathBuf> {
    let mut safe = PathBuf::new();
    for component in Path::new(relative_path).components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("unsafe relative path: {relative_path}");
            }
        }
    }

    if safe.as_os_str().is_empty() {
        bail!("empty relative path");
    }

    Ok(safe)
}

fn temp_part_path(final_path: &Path) -> PathBuf {
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("incoming");
    final_path.with_file_name(format!("{file_name}.borderless-part"))
}

fn mark_cancelled(cancelled: &Arc<Mutex<HashSet<Uuid>>>, transfer_id: Uuid) {
    if let Ok(mut cancelled) = cancelled.lock() {
        cancelled.insert(transfer_id);
    }
}

fn is_cancelled(cancelled: &Arc<Mutex<HashSet<Uuid>>>, transfer_id: Uuid) -> bool {
    cancelled
        .lock()
        .map(|cancelled| cancelled.contains(&transfer_id))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use borderless_core::file_transfer::{FileManifestEntry, FileTransferManifest};
    use std::{
        fs,
        net::TcpListener as StdTcpListener,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::{
        sync::mpsc,
        time::{timeout, Duration},
    };
    use uuid::Uuid;

    #[test]
    fn conflict_names_are_renamed_without_overwrite() {
        assert_eq!(
            rename_conflict("report.txt", 1),
            "report (Borderless 1).txt"
        );
        assert_eq!(rename_conflict("archive", 2), "archive (Borderless 2)");
    }

    #[test]
    fn expanded_directory_sources_keep_top_level_folder_name() {
        let root = temp_dir("expanded_directory_sources_keep_top_level_folder_name");
        let source_dir = root.join("docs");
        fs::create_dir_all(source_dir.join("nested")).unwrap();
        fs::write(source_dir.join("nested").join("note.txt"), b"note").unwrap();

        let files = expand_sources(&[source_dir.to_string_lossy().to_string()]).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, "docs/nested/note.txt");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_from_source_paths_expands_folders_and_totals_file_bytes() {
        let root = temp_dir("manifest_from_source_paths_expands_folders_and_totals_file_bytes");
        let source_dir = root.join("docs");
        fs::create_dir_all(source_dir.join("nested")).unwrap();
        fs::write(source_dir.join("nested").join("note.txt"), b"note").unwrap();
        fs::write(source_dir.join("root.txt"), b"root-file").unwrap();
        let transfer_id = Uuid::new_v4();

        let manifest =
            manifest_from_source_paths(transfer_id, &[source_dir.to_string_lossy().to_string()])
                .unwrap();

        assert_eq!(manifest.transfer_id, transfer_id);
        assert_eq!(manifest.root_name, "docs");
        assert_eq!(manifest.total_bytes, 13);
        assert!(manifest
            .files
            .iter()
            .any(|entry| entry.relative_path == "docs" && entry.is_dir));
        assert!(manifest
            .files
            .iter()
            .any(|entry| entry.relative_path == "docs/nested" && entry.is_dir));
        assert!(manifest
            .files
            .iter()
            .any(|entry| entry.relative_path == "docs/nested/note.txt" && entry.size_bytes == 4));
        assert!(manifest
            .files
            .iter()
            .any(|entry| entry.relative_path == "docs/root.txt" && entry.size_bytes == 9));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn loopback_directory_transfer_emits_completed_after_child_files() {
        let root = temp_dir("loopback_directory_transfer_emits_completed_after_child_files");
        let source_dir = root.join("docs");
        let server_cache = root.join("server-cache");
        let client_cache = root.join("client-cache");
        fs::create_dir_all(source_dir.join("nested")).unwrap();
        fs::create_dir_all(&server_cache).unwrap();
        fs::create_dir_all(&client_cache).unwrap();
        fs::write(source_dir.join("nested").join("note.txt"), b"note").unwrap();

        let transfer_id = Uuid::new_v4();
        let source_paths = vec![source_dir.to_string_lossy().to_string()];
        let manifest = manifest_from_source_paths(transfer_id, &source_paths).unwrap();
        let port = unused_tcp_port();
        let (server_events_tx, mut server_events_rx) = mpsc::unbounded_channel();
        let (server_commands_tx, server_commands_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client_commands_tx, client_commands_rx) = mpsc::unbounded_channel();

        let server = tokio::spawn(run_bulk_transfer_server(
            "127.0.0.1".to_string(),
            port,
            server_cache.to_string_lossy().to_string(),
            server_events_tx,
            server_commands_rx,
        ));
        let client = tokio::spawn(run_bulk_transfer_client(
            "127.0.0.1".to_string(),
            port,
            client_cache.to_string_lossy().to_string(),
            client_events_tx,
            client_commands_rx,
        ));

        client_commands_tx
            .send(BulkTransferCommand::SendFiles {
                manifest,
                source_paths,
            })
            .unwrap();

        let completed = timeout(Duration::from_secs(3), async {
            loop {
                if let Some(BulkTransferEvent::Completed {
                    transfer_id: id,
                    cache_paths,
                }) = server_events_rx.recv().await
                {
                    return (id, cache_paths);
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(completed.0, transfer_id);
        assert_eq!(
            completed.1,
            vec![server_cache.join("docs").to_string_lossy().to_string()]
        );
        assert_eq!(
            fs::read(server_cache.join("docs").join("nested").join("note.txt")).unwrap(),
            b"note"
        );

        let _ = client_commands_tx.send(BulkTransferCommand::Stop);
        let _ = server_commands_tx.send(BulkTransferCommand::Stop);
        client.await.unwrap().unwrap();
        server.await.unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn loopback_directory_transfer_renames_conflicting_top_level_folder() {
        let root = temp_dir("loopback_directory_transfer_renames_conflicting_top_level_folder");
        let source_dir = root.join("docs");
        let server_cache = root.join("server-cache");
        let client_cache = root.join("client-cache");
        fs::create_dir_all(source_dir.join("nested")).unwrap();
        fs::create_dir_all(server_cache.join("docs")).unwrap();
        fs::create_dir_all(&client_cache).unwrap();
        fs::write(source_dir.join("nested").join("note.txt"), b"note").unwrap();
        fs::write(server_cache.join("docs").join("existing.txt"), b"existing").unwrap();

        let transfer_id = Uuid::new_v4();
        let source_paths = vec![source_dir.to_string_lossy().to_string()];
        let manifest = manifest_from_source_paths(transfer_id, &source_paths).unwrap();
        let port = unused_tcp_port();
        let (server_events_tx, mut server_events_rx) = mpsc::unbounded_channel();
        let (server_commands_tx, server_commands_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client_commands_tx, client_commands_rx) = mpsc::unbounded_channel();

        let server = tokio::spawn(run_bulk_transfer_server(
            "127.0.0.1".to_string(),
            port,
            server_cache.to_string_lossy().to_string(),
            server_events_tx,
            server_commands_rx,
        ));
        let client = tokio::spawn(run_bulk_transfer_client(
            "127.0.0.1".to_string(),
            port,
            client_cache.to_string_lossy().to_string(),
            client_events_tx,
            client_commands_rx,
        ));

        client_commands_tx
            .send(BulkTransferCommand::SendFiles {
                manifest,
                source_paths,
            })
            .unwrap();

        let completed = timeout(Duration::from_secs(3), async {
            loop {
                if let Some(BulkTransferEvent::Completed {
                    transfer_id: id,
                    cache_paths,
                }) = server_events_rx.recv().await
                {
                    return (id, cache_paths);
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(completed.0, transfer_id);
        assert_eq!(
            completed.1,
            vec![server_cache
                .join("docs (Borderless 1)")
                .to_string_lossy()
                .to_string()]
        );
        assert_eq!(
            fs::read(
                server_cache
                    .join("docs (Borderless 1)")
                    .join("nested")
                    .join("note.txt")
            )
            .unwrap(),
            b"note"
        );
        assert_eq!(
            fs::read(server_cache.join("docs").join("existing.txt")).unwrap(),
            b"existing"
        );

        let _ = client_commands_tx.send(BulkTransferCommand::Stop);
        let _ = server_commands_tx.send(BulkTransferCommand::Stop);
        client.await.unwrap().unwrap();
        server.await.unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn loopback_empty_directory_transfer_preserves_folder() {
        let root = temp_dir("loopback_empty_directory_transfer_preserves_folder");
        let source_dir = root.join("empty-docs");
        let server_cache = root.join("server-cache");
        let client_cache = root.join("client-cache");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&server_cache).unwrap();
        fs::create_dir_all(&client_cache).unwrap();

        let transfer_id = Uuid::new_v4();
        let source_paths = vec![source_dir.to_string_lossy().to_string()];
        let manifest = manifest_from_source_paths(transfer_id, &source_paths).unwrap();
        let port = unused_tcp_port();
        let (server_events_tx, mut server_events_rx) = mpsc::unbounded_channel();
        let (server_commands_tx, server_commands_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client_commands_tx, client_commands_rx) = mpsc::unbounded_channel();

        let server = tokio::spawn(run_bulk_transfer_server(
            "127.0.0.1".to_string(),
            port,
            server_cache.to_string_lossy().to_string(),
            server_events_tx,
            server_commands_rx,
        ));
        let client = tokio::spawn(run_bulk_transfer_client(
            "127.0.0.1".to_string(),
            port,
            client_cache.to_string_lossy().to_string(),
            client_events_tx,
            client_commands_rx,
        ));

        client_commands_tx
            .send(BulkTransferCommand::SendFiles {
                manifest,
                source_paths,
            })
            .unwrap();

        let completed = timeout(Duration::from_secs(3), async {
            loop {
                if let Some(BulkTransferEvent::Completed {
                    transfer_id: id,
                    cache_paths,
                }) = server_events_rx.recv().await
                {
                    return (id, cache_paths);
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(completed.0, transfer_id);
        assert_eq!(
            completed.1,
            vec![server_cache
                .join("empty-docs")
                .to_string_lossy()
                .to_string()]
        );
        assert!(server_cache.join("empty-docs").is_dir());

        let _ = client_commands_tx.send(BulkTransferCommand::Stop);
        let _ = server_commands_tx.send(BulkTransferCommand::Stop);
        client.await.unwrap().unwrap();
        server.await.unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn loopback_transfer_writes_conflict_safe_cache_file() {
        let root = temp_dir("loopback_transfer_writes_conflict_safe_cache_file");
        let source_dir = root.join("source");
        let server_cache = root.join("server-cache");
        let client_cache = root.join("client-cache");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&server_cache).unwrap();
        fs::create_dir_all(&client_cache).unwrap();
        fs::write(server_cache.join("note.txt"), b"existing").unwrap();
        let source_file = source_dir.join("note.txt");
        fs::write(&source_file, b"bulk transfer payload").unwrap();

        let transfer_id = Uuid::new_v4();
        let manifest = FileTransferManifest {
            transfer_id,
            root_name: "note.txt".to_string(),
            files: vec![FileManifestEntry::file("note.txt", 21)],
            total_bytes: 21,
        };
        let port = unused_tcp_port();
        let (server_events_tx, mut server_events_rx) = mpsc::unbounded_channel();
        let (server_commands_tx, server_commands_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client_commands_tx, client_commands_rx) = mpsc::unbounded_channel();

        let server = tokio::spawn(run_bulk_transfer_server(
            "127.0.0.1".to_string(),
            port,
            server_cache.to_string_lossy().to_string(),
            server_events_tx,
            server_commands_rx,
        ));
        let client = tokio::spawn(run_bulk_transfer_client(
            "127.0.0.1".to_string(),
            port,
            client_cache.to_string_lossy().to_string(),
            client_events_tx,
            client_commands_rx,
        ));

        client_commands_tx
            .send(BulkTransferCommand::SendFiles {
                manifest,
                source_paths: vec![source_file.to_string_lossy().to_string()],
            })
            .unwrap();

        let completed = timeout(Duration::from_secs(3), async {
            loop {
                if let Some(BulkTransferEvent::Completed {
                    transfer_id: id,
                    cache_paths,
                }) = server_events_rx.recv().await
                {
                    return (id, cache_paths);
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(completed.0, transfer_id);
        assert_eq!(
            fs::read(server_cache.join("note (Borderless 1).txt")).unwrap(),
            b"bulk transfer payload"
        );
        assert_eq!(
            completed.1,
            vec![server_cache
                .join("note (Borderless 1).txt")
                .to_string_lossy()
                .to_string()]
        );

        let _ = client_commands_tx.send(BulkTransferCommand::Stop);
        let _ = server_commands_tx.send(BulkTransferCommand::Stop);
        client.await.unwrap().unwrap();
        server.await.unwrap().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn incoming_manifest_ignores_removed_target_directory_field() {
        let root = temp_dir("incoming_manifest_ignores_removed_target_directory_field");
        let cache_dir = root.join("server-cache");
        let stale_target = root.join("stale-target");
        let transfer_id = Uuid::new_v4();
        let manifest: FileTransferManifest = toml::from_str(&format!(
            r#"
transfer_id = "{transfer_id}"
root_name = "note.txt"
total_bytes = 4
target_directory = "{}"

[[files]]
relative_path = "note.txt"
size_bytes = 4
is_dir = false
"#,
            stale_target.to_string_lossy().replace('\\', "\\\\")
        ))
        .unwrap();
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let mut book = ReceiveBook::default();

        handle_manifest(&cache_dir, &events_tx, &mut book, manifest)
            .await
            .unwrap();

        let transfer = book.transfers.get(&transfer_id).unwrap();
        assert_eq!(
            transfer.cache_paths,
            vec![cache_dir.join("note.txt").to_string_lossy().to_string()]
        );
        assert!(!stale_target.exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("borderless-{name}-{}-{nanos}", std::process::id()));
        dir
    }

    fn unused_tcp_port() -> u16 {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }
}
