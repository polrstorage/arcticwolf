// NFS Protocol Implementation (NFSv3)
//
// This module implements the NFSv3 protocol procedures.
// See RFC 1813 for the complete specification.

pub mod dispatcher;
mod access;
mod create;
mod fsinfo;
mod fsstat;
mod getattr;
mod lookup;
mod null;
mod pathconf;
mod read;
mod readdir;
mod readdirplus;
mod setattr;
mod write;

pub use dispatcher::dispatch;
