//! curl -v \
//!     --header 'content-type: application/json' \
//!     --url 'http://127.0.0.1:4000' \
//!     --data '{"query":"query { topProducts { reviews { author { name } } name } }"}'
//! [...]
//! {"data":{"topProducts":[{"reviews":[{"author":{"name":"Ada Lovelace"}},{"author":{"name":"Alan Turing"}}],"name":"Table"},{"reviews":[{"author":{"name":"Ada Lovelace"}}],"name":"Couch"},{"reviews":[{"author":{"name":"Alan Turing"}}],"name":"Chair"}]}}
use anyhow::Result;

// adding the module to your main.rs file
// will automatically register it to the router plugin registry.
//
// you can use the plugin by adding it to `router.yml`
mod echo_co_processor;

// `APOLLO_KEY="YOUR_APOLLO_KEY" APOLLO_GRAPH_REF="YOUR_APOLLO_GRAPH_REF" cargo run -- -c ./router.yaml`
fn main() -> Result<()> {
    apollo_router::main()
}
