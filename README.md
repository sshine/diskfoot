# diskfoot

A terminal disk-usage tool shaped like three tools at once: the filesystem
summary of `df -h`, the per-directory accounting of `du`, and the lazy browser
of `ncdu`. It never reads file contents, so a directory listing is immediate and
a subtree is only walked when you expand it.

`df` answers only the filesystem question and says nothing about which directory
is responsible. `du` walks everything before it prints anything. `ncdu` is
interactive but still pays for the whole tree. `diskfoot` shows the filesystem
header at once, lists a directory's immediate children from metadata alone, and
computes each child's subtree in a background worker only when it is needed.

## Installation

```
nix run github:sshine/diskfoot
nix profile install github:sshine/diskfoot
```

You can download prebuilt [static binaries for x86_64 and aarch64 Linux][releases].
`diskfoot` is Linux-only: its size engine uses `statx`, `statvfs` and Linux
mount information.

## What the three faces mean

- **df face.** The header comes from `statvfs` on the selected path: mount point,
  filesystem type, total, used, available and use percent. It is computed for
  every draw and never walks the tree.
- **du face.** Each row is a directory entry and its subtree, measured from
  `st_blocks * 512` (allocated) and `st_size` (apparent). Directory inodes count
  their own blocks and size, as `du` counts them. Hard links are counted once per
  `(dev, ino)` within a walk, sparse files show their real allocation, and
  symlinks are counted as links, not followed.
- **ncdu face.** Expand a directory to list its children; the worker measures
  them in the background. Collapsing and re-expanding reuses the cache keyed by
  `(path, options)`. `r` clears it.

Rows start as `pending`. A directory that has not been expanded has not been
walked; a `partial` row could not be fully measured (depth limit, an entry on
another filesystem, or a read error); an `error` row could not be read, and the
reason appears in the status line.

## Fast paths, and the ext4 fallback

Before walking, `diskfoot` checks whether the filesystem can answer from its own
accounting. The fast path is used only for allocated sizes of a whole subtree.

| Filesystem | Detected when | Command | Supplies | Tested on a real filesystem here |
| --- | --- | --- | --- | --- |
| btrfs | `btrfs subvolume show <path>` succeeds | `btrfs filesystem du --raw` | allocated subtree bytes | no |
| ZFS | the path is a dataset mount point | `zfs list -Hp -o name,used,avail,mountpoint` | used bytes | no |
| XFS | `FS_IOC_FSGETXATTR` reports a project id | `xfs_quota -x -c 'quota -p -N -n -b -v <id>'` | used blocks × block size | no |
| ext4 and everything else | always | generic `statx` walk | allocated and apparent | yes |

The development host is ext4, so only the generic path has been exercised
end-to-end. The detection functions and every parser are unit-tested against
fixture command output; no test requires a btrfs, ZFS or XFS filesystem. A
missing tool, a non-zero exit, or output that does not parse quietly falls back
to the generic walk.

The fast paths report used bytes only; a directory's apparent size would require
walking it. Such a row shows `—` for apparent size (`null` in JSON) and keeps
`size_source` honest: `statx`, `btrfs`, `zfs` or `xfs`.

## CLI

`df`, `du` and `ncdu` in one lazy disk-usage tool.

The first screen is cheap: `statvfs` fills the filesystem header and a
directory read plus `statx` fills the child rows. A child's subtree is walked
only when you expand it, in a background worker, and the result is cached per
directory and option set.

### Usage

```console
$ diskfoot              # interactive TUI for the current directory
$ diskfoot /var         # interactive TUI for /var
$ diskfoot --json --depth 2 /var
```

`--json` is the non-interactive mode: it computes synchronously, emits a
stable object (see the JSON schema in the README), and never starts the TUI.
`--depth 0` reports the directory itself only; `--depth 1` adds its immediate
children; and so on. `--cross` includes filesystems other than the one the
target lives on; the default is one filesystem.

### TUI keys

| Key | Action |
| --- | --- |
| `q` | quit (or close help) |
| `?` | help |
| `enter`, `→`, `l` | expand the selected directory |
| `←`, `h` | collapse the selected directory, or move to its parent |
| `↑` `↓`, `k` `j` | move the selection |
| `r` | refresh: clear the cache, re-list and re-measure |
| `a` | toggle allocated (`st_blocks`) and apparent (`st_size`) sizes |
| `x` | toggle crossing filesystem boundaries |

Rows begin as `pending`. A directory that has not been expanded has not been
walked; a `partial` row could not be fully measured, whether because the JSON
depth limit was reached, an entry lived on another filesystem, or a read
failed; an `error` row could not be read, and the reason appears in the
status line.

### Size engine

Sizes come from metadata only: `st_blocks * 512` for allocated bytes and
`st_size` for apparent bytes. File contents are never read, symlinks are
never followed, sparse files report their real allocation, and hard links
are counted once per `(dev, ino)` within a walk.

### Exit codes

`0` on success, `1` on an I/O or mount-table error, `2` when the arguments
do not parse.

## JSON schema

`--json` computes synchronously and prints one object. The shape is stable;
`schema_version` will change if a field is removed or changes meaning.

```json
{
  "schema_version": 1,
  "path": "/home/user/data",
  "depth": 2,
  "options": { "cross_filesystems": false },
  "filesystem": {
    "mount_point": "/",
    "fs_type": "ext4",
    "total_bytes": 1000000000,
    "used_bytes": 400000000,
    "available_bytes": 600000000,
    "use_percent": 40.0
  },
  "allocated_bytes": 12345678,
  "apparent_bytes": 23456789,
  "size_source": "statx",
  "complete": true,
  "entries": [
    {
      "name": "src",
      "path": "/home/user/data/src",
      "kind": "directory",
      "allocated_bytes": 1234,
      "apparent_bytes": 2345,
      "size_source": "statx",
      "complete": true,
      "entries": [],
      "errors": []
    }
  ],
  "errors": [],
  "skipped_other_filesystems": 0,
  "state": "computed"
}
```

- `depth` is echoed from the request. `--depth 0` reports the directory itself
  only, so `entries` is empty and `complete` is `false`.
- `entries` nests one object per level, up to `depth`. `kind` is `file`,
  `directory`, `symlink` or `other`.
- `allocated_bytes` comes from `st_blocks`; `apparent_bytes` from `st_size`, and
  is `null` when a fast path supplied used bytes only.
- `size_source` is `statx`, `btrfs`, `zfs` or `xfs`.
- `complete` is false when the depth limit was reached, an entry on another
  filesystem was skipped, or a permission error stopped part of the tree.
- `errors` lists per-path messages. The walk continues past them.
- `state` is `computed` for a synchronous JSON run. The TUI's `pending` and
  `error` states are visible there, not in this field.
- `skipped_other_filesystems` counts entries excluded because they live on a
  different `st_dev` than the root; pass `--cross` to include them.

## License

MIT OR Apache-2.0.

[releases]: https://github.com/sshine/diskfoot/releases
