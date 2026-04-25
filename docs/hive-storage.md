# Hive Storage: Tiered Knowledge with Cloud Backends

Status: Draft

## Problem

The hive knowledge store has a hard cap (default 500 units) to prevent unbounded memory growth. This means valuable learnings are evicted when the store is full. Teams accumulating knowledge across many peers and projects lose signal over time.

We need unbounded learning capacity without unbounded memory usage, plus a distillation pipeline that condenses raw knowledge into formats optimized for safe agent autonomy.

## Architecture: Three-Tier Knowledge

```
┌──────────────────────────────────────────────────────────┐
│  HOT TIER (brain prompt)                                 │
│  Top 20 units by confidence × evidence                   │
│  In-memory, rebuilt every brain evaluation                │
│  ~2KB of prompt text                                     │
├──────────────────────────────────────────────────────────┤
│  WARM TIER (local store)                                 │
│  Up to 500 active units in ~/.claudectl/hive/            │
│  HashMap in memory, JSONL on disk                        │
│  Compacted every distillation cycle                      │
├──────────────────────────────────────────────────────────┤
│  COLD TIER (cloud storage)                               │
│  Unbounded archive of all knowledge ever generated       │
│  Periodically distilled into condensed curriculum         │
│  Pull-on-demand: warm tier requests specific knowledge   │
└──────────────────────────────────────────────────────────┘
```

## Storage Backend Abstraction

A `StorageBackend` trait allows plugging in different cold storage providers:

```rust
trait StorageBackend: Send + Sync {
    /// Push knowledge units to cold storage.
    fn push(&self, units: &[KnowledgeUnit]) -> Result<u32, String>;
    /// Pull units matching a query (scope, category, tool, min_confidence).
    fn pull(&self, query: &StorageQuery) -> Result<Vec<KnowledgeUnit>, String>;
    /// Trigger distillation on the cold store (merge duplicates, condense).
    fn distill(&self) -> Result<DistillationReport, String>;
    /// Get storage stats (total units, size, last distillation).
    fn stats(&self) -> Result<StorageStats, String>;
}
```

### Built-in Backends

| Backend | Config key | Description |
|---------|-----------|-------------|
| `local` | `storage = "local"` | Default. Cold tier is a second JSONL file (`archive.jsonl`). No cloud. |
| `s3` | `storage = "s3"` | AWS S3 / S3-compatible (MinIO, R2, Backblaze B2). |
| `gcs` | `storage = "gcs"` | Google Cloud Storage. |
| `git` | `storage = "git"` | Committed to a git repo (team-shared knowledge base). |

### Configuration

```toml
[hive.storage]
backend = "local"               # "local", "s3", "gcs", "git"
auto_archive = true             # push evicted units to cold storage on compaction
distill_interval_hours = 24     # how often to run cold distillation
pull_on_miss = true             # when brain needs knowledge not in warm tier, query cold

# S3 backend
[hive.storage.s3]
bucket = "team-claudectl-hive"
prefix = "knowledge/"
region = "us-east-1"
# Credentials via AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY env vars

# GCS backend
[hive.storage.gcs]
bucket = "team-claudectl-hive"
prefix = "knowledge/"
# Credentials via GOOGLE_APPLICATION_CREDENTIALS env var

# Git backend
[hive.storage.git]
repo = "/path/to/shared-repo"
branch = "hive"
path = ".claudectl/knowledge/"
```

## Lifecycle: From Raw Signal to Distilled Curriculum

```
Raw decisions (brain/decisions.jsonl)
         │
         ▼ every 10 decisions
┌────────────────────┐
│ Local distillation  │  → PreferencePatterns, ToolAccuracy, Insights
└────────┬───────────┘
         │
         ▼
┌────────────────────┐
│ Knowledge units     │  → Warm tier (up to 500 in local store)
└────────┬───────────┘
         │ on compaction (evicted units)
         ▼
┌────────────────────┐
│ Cold archive        │  → Cloud storage (unbounded)
└────────┬───────────┘
         │ every 24 hours
         ▼
┌────────────────────────────────────┐
│ Cold distillation                   │
│                                    │
│ 1. Dedup: merge units with same    │
│    semantic key, keep highest      │
│    confidence × evidence           │
│                                    │
│ 2. Condense: collapse similar      │
│    patterns into broader rules     │
│    (e.g., "approve cargo test"     │
│    + "approve cargo clippy"        │
│    → "approve cargo *")            │
│                                    │
│ 3. Prune: remove contradicted      │
│    units (high-confidence deny     │
│    supersedes low-confidence       │
│    approve for same tool/command)  │
│                                    │
│ 4. Curriculum: produce a compact   │
│    "best of" knowledge set that    │
│    can be loaded into the warm     │
│    tier on any machine             │
└────────┬───────────────────────────┘
         │
         ▼
┌────────────────────┐
│ Distilled curriculum│  → Compact, high-signal knowledge
│ (~50-100 units)     │  → Can be pulled into warm tier
└────────────────────┘
```

## Cold Distillation: Condensing Knowledge

The cold distillation pipeline runs periodically (default every 24 hours) on the archive. Its goal: reduce thousands of raw knowledge units into a compact curriculum that maximizes safe autonomy while minimizing drift.

### Step 1: Deduplication

Multiple peers may generate semantically identical knowledge. The semantic key system already handles exact matches, but cold distillation also merges near-duplicates:

- Same tool + overlapping command patterns → keep the more general one
- Same insight category + similar summaries → merge evidence counts
- Same temporal pattern from multiple peers → combine into one with aggregated evidence

### Step 2: Condensation

Similar patterns collapse into broader rules:

```
Before:
  [Bash, "cargo test"] approve (95%, 20 evidence)
  [Bash, "cargo clippy"] approve (92%, 15 evidence)
  [Bash, "cargo fmt"] approve (98%, 25 evidence)
  [Bash, "cargo build"] approve (90%, 12 evidence)

After condensation:
  [Bash, "cargo *"] approve (94%, 72 evidence)  ← combined
```

Condensation only fires when 3+ patterns share a common prefix and all have > 80% accept rate. The condensed rule carries the sum of evidence counts and the weighted average confidence.

### Step 3: Contradiction Resolution

When the archive contains conflicting knowledge (e.g., one peer says approve, another says deny for the same tool/command), the distiller resolves:

- Higher confidence × evidence wins
- If close (within 10%), flag as "disputed" — excluded from curriculum
- Contradictions are logged for human review

### Step 4: Curriculum Generation

The final output is a compact curriculum (target: 50-100 units) organized by:

1. **Safety guards** — deny rules with high confidence (always included)
2. **Best practices** — approve patterns with high confidence + evidence
3. **Techniques** — error handling, context management patterns
4. **Workflow** — model selection, delegation strategies

The curriculum is versioned and timestamped. Machines can pull the latest curriculum to bootstrap or refresh their warm tier.

## Drift Prevention

The distillation pipeline is designed to prevent the hive mind from drifting toward unsafe behavior:

1. **Deny-first curriculum**: Safety guards (deny rules) are always included in the curriculum regardless of evidence count. A single well-justified deny rule cannot be outvoted by many low-quality approve rules.

2. **Confidence floor**: Only units with confidence > 0.7 enter the curriculum. Low-confidence patterns stay in the archive but don't influence the brain.

3. **Contradiction flagging**: When peers disagree, the disputed knowledge is excluded rather than averaged. This prevents a rogue peer from shifting the group consensus.

4. **Curriculum versioning**: Each curriculum has a version and hash. The warm tier tracks which version it's running. If a new curriculum introduces regressions (the local brain starts rejecting more), the user can pin to a previous version.

5. **Local override**: The local brain's own preferences ALWAYS override the curriculum. The curriculum is advisory, never authoritative.

## Implementation: Local Backend (Phase 1)

The local backend implements the storage trait using a second JSONL file (`archive.jsonl`):

```
~/.claudectl/hive/
  knowledge.jsonl       ← warm tier (active, bounded)
  archive.jsonl         ← cold tier (evicted units, unbounded on disk)
  curriculum.json       ← latest distilled curriculum
  curriculum_meta.json  ← version, timestamp, hash, unit count
```

This gives the full lifecycle without any cloud dependency. The archive grows on disk (which is cheap) while memory stays bounded.

### Archive on compaction

When `compact()` evicts units, they're appended to `archive.jsonl` instead of being deleted:

```rust
pub fn compact_with_archive(&mut self, ...) -> CompactionResult {
    let evicted = self.compact(...);
    if !evicted.is_empty() && auto_archive {
        archive_units(&evicted);  // append to archive.jsonl
    }
    // ...
}
```

### Pull on miss

When the brain needs knowledge about a specific tool/context and the warm tier doesn't have it, it can query the archive:

```rust
if warm_store.find_by_tool("docker").is_empty() {
    let archived = archive.pull(&StorageQuery { tool: Some("docker"), .. });
    // Promote the highest-confidence archived unit to warm tier
}
```

### Periodic distillation

A background thread (or CLI command) runs distillation:

```bash
claudectl --hive distill          # manual trigger
claudectl --hive curriculum       # show current curriculum
claudectl --hive "curriculum --pull"  # pull latest into warm tier
```

## CLI

```bash
# Archive management
claudectl --hive archive          # show archive stats
claudectl --hive "archive --prune 90d"  # prune archive entries older than 90 days

# Distillation
claudectl --hive distill          # run cold distillation now
claudectl --hive curriculum       # show current curriculum
claudectl --hive "curriculum --pull"    # pull curriculum into warm tier
claudectl --hive "curriculum --pin v3"  # pin to a specific curriculum version

# Storage backend
claudectl --hive "storage status"     # show backend config and stats
claudectl --hive "storage push"       # manually push warm tier to cold
claudectl --hive "storage pull"       # manually pull from cold to warm
```

## Future: Cloud Backends

Cloud backends (S3, GCS, git) follow the same trait but store the archive remotely. They add:

- **Team-wide curriculum**: Multiple machines push to the same bucket, distillation runs server-side or on a designated machine
- **Async sync**: Machine B gets knowledge from Machine A even when A is offline
- **Versioned history**: Every curriculum version is retained in the bucket
- **Access control**: IAM/ACL controls who can push/pull

These are Phase 2 — the local backend provides the full architecture without cloud dependencies.

## Future: Training & Inference Compute

The cold storage layer is the foundation for future compute provisioning:

- **Training**: Fine-tune a small model on the distilled curriculum to create a project-specific brain that doesn't need prompt injection
- **Inference**: Run the fine-tuned model as the brain instead of (or alongside) the general ollama model
- **Evaluation**: Test candidate curricula against the brain eval harness before deploying

These are Phase 3 — they require the storage and distillation pipeline to be stable first.
