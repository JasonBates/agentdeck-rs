//! Transcript filesystem adapters with bounded, injected blocking I/O.

mod filesystem;

pub use filesystem::{
    BlockingTranscriptIo, BlockingTranscriptRead, CodexScanLimits, FileTimestamp,
    FilesystemTranscriptSource, StdTranscriptIo, TranscriptAdapterLimits, TranscriptIoError,
    TranscriptObservation, TranscriptRequest, TranscriptRoots, TranscriptSource,
    TranscriptSourceBuildError, TranscriptWindows,
};
