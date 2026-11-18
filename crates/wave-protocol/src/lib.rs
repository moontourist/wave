//! Shared, platform-neutral protocol types for Wave.
//!
//! The server owns the runtime model and the client owns the Mach transport,
//! but both use these types at the wire boundary. JSON encoding remains an
//! implementation detail of the transport crates.

extern crate self as wave_protocol;

mod commands;
mod events;
mod layout;
mod queries;
mod selectors;
mod transport;

pub use commands::{
    AnimationEasing, CommandArgument, ConfigCommand, FloatingWindowSize, FloatingWindowSizePreset,
    LayoutCommand, MetricsCommand, ReactorCommand, ToggleWindowFloatingOptions, WaveCommand,
};
pub use events::{EventKind, StackInfo, WaveEvent, WorkspaceId};
pub use layout::{
    Direction, DirectionalDistance, LayoutKind, LayoutMode, Orientation, ParseDirectionError,
    ResizeOrientation,
};
pub use queries::{
    ApplicationData, ContainerNodeType, ContainerTreeNode, DisplayData, LayoutStateData, Point,
    Rect, Size, WindowData, WindowId, WindowLayoutPosition, WorkspaceData, WorkspaceLayoutData,
};
pub use selectors::{DisplaySelector, RestoreScope, RestoreSource, WorkspaceSelector};
pub use transport::{JsonWaveResponse, WaveRequest, WaveResponse, WindowClaimFlags};
