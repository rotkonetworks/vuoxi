//! The backup tier: files erasure-coded into ZODA shards and spread across
//! hosts, so any n of n+k rebuild everything and every shard is verifiable on
//! arrival. Runs in the background on mains power only.

#![forbid(unsafe_code)]
