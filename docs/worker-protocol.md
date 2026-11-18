# Private worker protocol

Only `kanade-mozc` and `worker_main.cc` use this protocol. Frontends use the separate Kanade IPC protocol. Worker manifests require `worker_protocol = 2` in addition to the pinned Mozc commit, tag and binary hash.

Frames retain the existing little-endian 32-bit length, with a one-MiB limit. Ordinary frames contain unmodified Mozc `Input` and `Output` protobuf messages. Generated Mozc types stay private to the adapter.

Dictionary controls have a 17-byte body. Their prefix is `00 4b 41 4e 41 44 45 01` (`NUL`, `KANADE`, version 1). A zero protobuf field tag is invalid, so these frames cannot be mistaken for ordinary Mozc messages.

| Bytes | Request | Reply |
|---|---|---|
| 0–7 | Prefix | Prefix |
| 8 | Operation: 1 starts a load, 2 reads status | State: 0 idle, 1 loading, 2 ready, 3 failed, 4 busy |
| 9–16 | Nonzero generation for a load; zero for status | Current generation, as a little-endian `u64` |

The worker accepts one load at a time. Repeating its generation returns the current or completed result without loading a changed file again. A different generation receives `busy` while a load is running. A restarted worker begins with generation zero; callers must not mistake that for completion of an earlier operation.

The background thread reads only `user_dictionary.db` in the worker's private profile, with an eight-MiB limit. It waits for the initial upstream dictionary reloader, parses the private storage protobuf and calls the dictionary interface's loader. Token-index publication uses Mozc's existing atomic shared snapshot. `ready` means that this load completed successfully, not that every possible entry is valid or reachable during conversion. Management code must validate entries before publication.

This path does not invoke general `Engine::Reload` or reload conversion/prediction history. It needs no history synchronization. General Mozc reload still requires `SYNC_DATA` first. The loader is joined before the owning engine is destroyed. Control frames and diagnostic output contain no vocabulary or input text.

This backend does not itself authorize or publish dictionary edits. The authenticated [management service](dictionary-management.md) compares the expected file revision, stages writes away from the key path and refuses publication while composition or an unacknowledged edit is active. It also waits for two seconds without input, retains the writer lock through loading, and prevents publication while a previous dictionary load is active. A failed load is retried with a fresh generation, at most three attempts over thirty seconds. The saved revision is retained if loading or directory synchronization fails. No general history reload or rollback of a validated user edit is performed.
