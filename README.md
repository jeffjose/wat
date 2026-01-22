# wat

Watch what LLM agents are doing to your files.

```
 wat | /path/to/project [building rust /]              14:32:05
------------------------------------------------------------------
GIT  +123   -45     *3 +1

COMMIT
  a94a6a6    5m
  Add new feature
  src/main.rs, src/lib.rs

CHANGES
  ~ src/main.rs                              +45  -12  ++++++----
  + src/new_file.rs                          +120 -0   ++++++++
```

## Install

```
cargo install --path .
```

## Usage

```
wat [path]
```

Press `q` to quit.
