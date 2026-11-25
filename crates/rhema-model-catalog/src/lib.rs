//! The live model-facts catalog.
//!
//! The adapters' live listings say what an account can run; they do
//! not all say what those models cost, how much context they take, or
//! whether they read images. This crate fetches the published facts
//! from models.dev on first use, caches them in memory and on disk,
//! and fills the blanks the listings leave. The listing stays the
//! authority: [`Catalog::decorate`] writes only over an empty field.
//!
//! Layout:
//! - `catalog`: the projection, the provider table, and the lookup
//! - the live module (feature `live`): the fetch, the disk cache, and
//!   the TTL
//! - `decorate`: the [`ModelListing`](rhema_provider::ModelListing)
//!   wrapper the provider registry installs
#![forbid(unsafe_code)]

mod catalog;
mod decorate;
#[cfg(feature = "live")]
mod live;

pub use catalog::{Catalog, Facts, provider_key};
pub use decorate::decorate_listing;
#[cfg(feature = "testing")]
pub use live::install_for_tests;
#[cfg(feature = "live")]
pub use live::{CatalogCache, DEFAULT_URL, Fetched, ModelsDev, Source};
