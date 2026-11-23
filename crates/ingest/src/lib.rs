//! The write path (SPEC section 3): the host decides the facts, and `HostCommitter` validates them,
//! resolves entities through `entities` and gates them through `gate`. Page writes (SPEC section 8)
//! go through `PageWriter`.

mod entities;
mod gate;
mod host;
mod pages;
mod redact;
mod single;
mod validate;

pub use host::{
    CommitFact, CommitRequest, CommitResponse, CommittedFact, EntityRef, HostCommitter, RelatedFact,
};
pub use pages::{
    split_sections, PageWrite, PageWriteOutcome, PageWriter, SplitSection, PAGE_HEADING_MAX,
};
pub use redact::redact;
pub use single::{normalize_predicate, SingleValued, DEFAULT_SINGLE_VALUED_PREDICATES};
