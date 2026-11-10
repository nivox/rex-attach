//! Browser state, rendering, and asynchronous effects.
mod controller;
mod data;
mod dialog;
mod model;
mod state;
mod view;

pub use controller::{Request, browse};
pub use state::{Browser, PreviewLayout};

#[cfg(test)]
mod tests;
