//! Engine routing: the one `match SearchProvider` from a fan-out leg to the
//! engine's own search function, next to the engines themselves.

use crate::web::search::bing::bing_search;
use crate::web::search::brave::brave_search;
use crate::web::search::cratesio::cratesio_search;
use crate::web::search::ddg::ddg_search;
use crate::web::search::governor::EnginePermit;
use crate::web::search::startpage::startpage_search;
use crate::web::search::types::{Recency, SearchProvider, SearchProviderError, SearchResult};
use crate::web::search::yahoo::yahoo_search;

/// Base URLs for every engine: production callers pass the real hosts,
/// tests pass local `TcpListener` stubs.
pub(super) struct EngineBases {
    pub(super) ddg: String,
    pub(super) sp_home: String,
    pub(super) sp_search: String,
    pub(super) brave: String,
    pub(super) yahoo: String,
    pub(super) bing: String,
    pub(super) cratesio: String,
}

/// Run one `query` against `provider`'s own search function. `permit` is
/// the leg's held engine permit, already paced for the first request;
/// engines that send follow-up requests inside one leg pace them through
/// it (#104: DDG's continuation POSTs).
pub(super) async fn search_engine(
    client: &reqwest::Client,
    provider: SearchProvider,
    query: &str,
    recency: Option<Recency>,
    bases: &EngineBases,
    permit: &EnginePermit<'_>,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    match provider {
        SearchProvider::Startpage => {
            startpage_search(client, query, recency, &bases.sp_home, &bases.sp_search).await
        }
        SearchProvider::DuckDuckGo => {
            ddg_search(client, query, recency, &bases.ddg, Some(permit)).await
        }
        SearchProvider::Brave => brave_search(client, query, &bases.brave).await,
        SearchProvider::Yahoo => yahoo_search(query, recency, &bases.yahoo).await,
        SearchProvider::Bing => bing_search(client, query, &bases.bing).await,
        SearchProvider::CratesIo => cratesio_search(client, query, &bases.cratesio).await,
    }
}
