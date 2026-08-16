pub(crate) mod adapters;
pub(crate) mod catalog;
pub(crate) mod classifier;
pub(crate) mod domain;
pub(crate) mod runtime;
pub(crate) mod semantic;
pub(crate) mod service;

pub(crate) use catalog::ProjectCatalog;
pub(crate) use domain::*;
pub(crate) use service::ProjectService;
