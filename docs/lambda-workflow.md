# Lambda Workflow

This document shows the current runtime workflow for the `RustBucketDeployment` provider Lambda.

## GitHub Theme Support

The diagrams below use GitHub-flavored Markdown Mermaid code blocks instead of static images, so GitHub renders them in the viewer's current light or dark theme. If these diagrams are ever exported to image files, use GitHub's theme-aware `<picture>` pattern:

```html
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagram-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="diagram-light.png">
  <img alt="Workflow diagram" src="diagram-light.png">
</picture>
```

## Handler Overview

```mermaid
flowchart TD
  A["Lambda cold start"] --> B["Load AWS config"]
  B --> C["Create shared clients: S3, CloudFront, HTTP"]
  C --> D["Register lambda_runtime service_fn"]
  D --> E["Receive CloudFormation custom resource event"]
  E --> F["Deserialize event into typed CloudFormation request"]
  F --> G{"Request type"}

  G -->|Create| H["Generate new PhysicalResourceId"]
  G -->|Update| I["Reuse existing PhysicalResourceId"]
  G -->|Delete| J["Reuse existing PhysicalResourceId"]

  H --> K["Parse ResourceProperties into DeploymentRequest"]
  I --> K
  J --> K

  K --> L{"Delete and retainOnDelete=false?"}
  L -->|Yes| M["Check bucket ownership tag"]
  M --> N{"Bucket owned by this custom resource?"}
  N -->|No| O["Delete destination prefix"]
  N -->|Yes| P["Skip delete prefix"]
  L -->|No| Q{"Create or Update?"}
  O --> Q
  P --> Q

  Q -->|Yes| R["Run S3 deployment"]
  Q -->|No| S["Skip S3 deployment"]

  R --> T{"Distribution configured?"}
  S --> T
  T -->|Yes| U["Create CloudFront invalidation"]
  U --> V{"waitForDistributionInvalidation?"}
  V -->|Yes| W["Poll GetInvalidation until Completed or timeout"]
  V -->|No| X["Return after CreateInvalidation"]
  T -->|No| Y["Skip CloudFront"]

  W --> Z{"Update and destination changed and retainOnDelete=false?"}
  X --> Z
  Y --> Z
  Z -->|Yes| AA["Delete old destination prefix"]
  Z -->|No| AB["Build response data"]
  AA --> AB

  AB --> AC["PUT SUCCESS response to CloudFormation ResponseURL"]
  F -. "parse or runtime error" .-> AD["Build FAILED response with error chain"]
  K -. "deployment error" .-> AD
  R -. "S3 error" .-> AD
  U -. "CloudFront error" .-> AD
  W -. "timeout" .-> AD
  AD --> AE["PUT FAILED response to CloudFormation ResponseURL"]
```

## S3 Deployment Workflow

```mermaid
flowchart TD
  A["deploy(state, request)"] --> B["Validate source array lengths"]
  B --> C["Compile include and exclude glob filters"]
  C --> D["Build object metadata from request"]
  D --> E{"extract?"}

  E -->|true| F["For each source object: GetObject source zip"]
  F --> G{"ContentLength <= 256 MiB?"}
  G -->|Yes| H["Read source zip into memory"]
  G -->|No or unknown| I["Stream source zip to /tmp"]
  H --> J["Open ZipArchive reader"]
  I --> J
  J --> K["Walk zip entries"]
  K --> L{"Entry is file and passes filters?"}
  L -->|No| K
  L -->|Yes| M["Add planned ZipEntry with archive index, entry index, CRC32, and size"]
  M --> K

  E -->|false| N["For each source object: HeadObject source"]
  N --> O["Record source ETag as expected content hash"]
  O --> P["Add planned CopyObject"]

  K --> Q["List destination prefix with ListObjectsV2"]
  P --> Q
  Q --> R["Record destination key to size, checksum hints, and ETag map"]
  Q --> S{"prune=true and destination key missing from plan?"}
  S -->|Yes| T["Queue key for DeleteObjects"]
  S -->|No| U["Keep key"]
  T --> V{"extract?"}
  U --> V

  V -->|false| W["Build copy plans"]
  W --> X{"Source ETag matches destination ETag?"}
  X -->|Yes| Y["Skip CopyObject"]
  X -->|No| Z["CopyObject with MetadataDirective=REPLACE"]
  Z --> AA["Run copies with up to 8 parallel transfers"]
  Y --> AA

  V -->|true| AB["Group zip entries by source archive"]
  AB --> AC["Open ZipArchive from memory or temporary file"]
  AC --> AD["Read planned entry"]
  AD --> AE{"Source has deploy-time markers?"}

  AE -->|Yes| AF["Read full entry into memory"]
  AF --> AG["Apply marker replacement"]
  AG --> AH["MD5 and CRC32 final replaced bytes"]
  AH --> AI{"MD5 equals destination ETag?"}
  AI -->|Yes| AJ["Skip PutObject"]
  AI -->|No| AK["PutObject replaced bytes with x-amz-checksum-crc32"]

  AE -->|No| AL{"Entry >= 8 MiB and destination size/CRC32 metadata can be checked?"}
  AL -->|Yes| AM["HeadObject with ChecksumMode=Enabled"]
  AM --> AN{"ChecksumCRC32 equals zip CRC32?"}
  AN -->|Yes| AO["Skip PutObject"]
  AN -->|No| AP["Create retryable S3 body with x-amz-checksum-crc32"]
  AL -->|No| AQ{"Entry <= 32 MiB?"}
  AQ -->|Yes| AR["Read decompressed entry once into memory and compute MD5"]
  AQ -->|No| AS["Read entry in 8 MiB chunks and compute MD5"]
  AR --> AT{"MD5 equals destination ETag?"}
  AS --> AT
  AT -->|Yes| AO
  AT -->|No| AU{"Cached bytes available?"}
  AU -->|Yes| AV["PutObject cached bytes with x-amz-checksum-crc32"]
  AU -->|No| AP["Create retryable S3 body with x-amz-checksum-crc32"]
  AP --> AW["Stream entry to S3 in 8 MiB chunks"]

  AK --> AX["Run uploads with up to 8 parallel transfers"]
  AV --> AX
  AW --> AX
  AJ --> AX
  AO --> AX

  AA --> AY
  AX --> AY{"prune=true?"}
  AY -->|Yes| AZ["Delete queued keys with DeleteObjects in 1000-key chunks"]
  AY -->|No| BA["Deployment complete"]
  AZ --> BA
```

## Skip Decision Path

```mermaid
flowchart LR
  A["Planned object"] --> B["Destination ListObjectsV2 metadata"]
  B --> C{"Marker-free zip entry >= 8 MiB with destination CRC32 FULL_OBJECT and matching size?"}
  C -->|Yes| D["HeadObject with ChecksumMode=Enabled"]
  D --> E{"ChecksumCRC32 equals zip CRC32?"}
  E -->|Yes| F["Skip upload"]
  E -->|No| G["Upload"]
  C -->|No| H{"ETag fallback available?"}
  H -->|Yes| I{"Expected ETag equals destination ETag?"}
  I -->|Yes| F
  I -->|No| G
  H -->|No| G

  J["extract=false"] --> K["Expected ETag from source HeadObject"]
  L["extract=true without markers"] --> M["Expected CRC32 + size from zip central directory"]
  N["extract=true with markers"] --> O["Expected ETag from MD5 after replacement"]

  K --> A
  M --> A
  O --> A
```

## File Upload Handling

The destination objects are listed once per deployment after the source plan is built. Key, size, checksum algorithm/type, and `ETag` metadata are stored in memory as a key-to-metadata map, not as upload payloads.

```mermaid
flowchart TD
  A["Start S3 deployment"] --> B{"extract?"}

  B -->|true| C["Keep source zip in memory up to 256 MiB, otherwise stream to /tmp"]
  C --> D["Walk archive entries"]
  D --> E["Build source manifest: relative key -> zip entry location"]

  B -->|false| F["HeadObject each source object"]
  F --> G["Build source manifest: relative key -> source object + source ETag"]

  E --> H["List destination prefix once with ListObjectsV2"]
  G --> H
  H --> I["Store destination objects in memory"]
  I --> J["HashMap: relative key -> size, checksum hints, ETag"]
  J --> K{"Planned item type"}

  K -->|CopyObject extract=false| L["Read expected ETag from source HeadObject"]
  L --> M{"Expected ETag equals destination ETag?"}
  M -->|Yes| N["Skip CopyObject"]
  M -->|No| O["CopyObject source to destination"]

  K -->|Zip entry without markers| P{"Entry >= 8 MiB, destination size matches, and advertises CRC32 FULL_OBJECT?"}
  P -->|Yes| Q["HeadObject with ChecksumMode=Enabled"]
  Q --> R{"ChecksumCRC32 equals zip CRC32?"}
  R -->|Yes| T["Skip PutObject"]
  R -->|No| U["Create retryable upload body with x-amz-checksum-crc32"]
  P -->|No| V{"Entry <= 32 MiB?"}
  V -->|Yes| W["Read decompressed entry once into memory and compute MD5"]
  V -->|No| X["Read entry in 8 MiB chunks and compute MD5"]
  W --> Y{"MD5 equals destination ETag?"}
  X --> Y
  Y -->|Yes| T
  Y -->|No| Z{"Cached bytes available?"}
  Z -->|Yes| AA["PutObject cached bytes"]
  Z -->|No| U
  U --> AB["Stream PutObject body from source archive"]

  K -->|Zip entry with markers| AC["Read full entry into memory"]
  AC --> AD["Apply marker replacement"]
  AD --> AE["Compute MD5 and CRC32 of replaced bytes"]
  AE --> AF{"MD5 equals destination ETag?"}
  AF -->|Yes| AG["Skip PutObject"]
  AF -->|No| AH["PutObject replaced bytes with x-amz-checksum-crc32"]

  O --> AI["Copy/checksum/upload concurrency bounded to 8"]
  AA --> AI
  AB --> AI
  AH --> AI
  N --> AJ["Item complete"]
  T --> AJ
  AG --> AJ
  AI --> AJ
```

For plain zip entries, the handler prefers zip CRC32 plus uncompressed size against S3 full-object CRC32 metadata only for entries at least 8 MiB. When that is available, unchanged entries are skipped without decompressing the entry. Smaller entries and entries without usable checksum metadata fall back to MD5 and compare against the destination ETag map. Fallback entries up to 32 MiB are cached after decompression so changed entries can upload those same bytes; larger entries keep the streaming 8 MiB chunk path. Checksum reads, fallback hashing, and uploads run inside the bounded transfer task pool.

## Current Runtime Notes

- Source zip archives are kept in memory when S3 reports `ContentLength <= 256 MiB`; larger or unknown-size archives are streamed to temporary files in Lambda `/tmp`.
- Plain zip entries at least 8 MiB use zip CRC32 and S3 checksum metadata when available. If changed, the upload stream reopens the entry from the retained source archive and sends one 8 MiB chunk at a time with `x-amz-checksum-crc32`.
- In the MD5 fallback path, decompressed entries up to 32 MiB are cached and uploaded from those cached bytes if changed; larger entries are streamed for hashing and streamed again only if upload is needed.
- The upload stream is retryable because the body can be rebuilt from the retained in-memory or temporary source archive.
- Zip entries with deploy-time replacements are still fully materialized in memory after replacement, because the final bytes must be known before computing the ETag/CRC32 and uploading.
- The handler does not extract the archive to disk and does not stage individual zip entries in `/tmp`.
- Copy, checksum read, fallback hash, and upload work is bounded by `MAX_PARALLEL_TRANSFERS = 8`.
- `prune=true` lists the destination prefix and deletes destination objects that are not in the planned source set.
- CloudFront invalidation is created after S3 deployment or delete handling; if waiting is enabled, the handler polls until completion or timeout.

## Memory Budget

The construct default is `DEFAULT_MEMORY_LIMIT_MB = 1024`. The current Rust constants are budgeted against that 1 GiB Lambda size:

```text
peak ~= runtime_reserve
      + MEMORY_ARCHIVE_THRESHOLD_BYTES
      + MAX_PARALLEL_TRANSFERS
        * (ZIP_ENTRY_READ_CHUNK_BYTES
           + DECOMPRESSED_ENTRY_CACHE_THRESHOLD_BYTES
           + sdk_http_overhead)
      + safety_margin

941 MiB ~= 205 MiB
        + 256 MiB
        + 8 * (8 MiB + 32 MiB + 4 MiB)
        + 128 MiB
```

`runtime_reserve` is estimated as roughly 20% of the Lambda memory for AL2023, the Rust runtime, SDK clients, allocator behavior, and process overhead. `sdk_http_overhead` is an estimate for per-transfer SDK/body/http buffering. The 128 MiB safety margin covers variance, destination metadata maps, futures, logging, and smaller temporary allocations. These are not hard AWS guarantees; they are the sizing assumptions behind keeping `MEMORY_ARCHIVE_THRESHOLD_BYTES` at 256 MiB and `DECOMPRESSED_ENTRY_CACHE_THRESHOLD_BYTES` at 32 MiB with eight concurrent transfers.

`REMOTE_CHECKSUM_MIN_BYTES = 8 MiB` is separate from the memory budget. It reflects the benchmark result that checksum-mode `HeadObject` is slower than local MD5 for small files while S3 `ListObjectsV2` exposes checksum presence but not the actual CRC32 value. If S3 starts returning actual CRC32 values in `ListObjectsV2`, the local-MD5 threshold should be removed because no extra API call would be needed for CRC32 comparisons.
