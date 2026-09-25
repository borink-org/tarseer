# tarseer

Walks a directory tree and builds an index, as JSONL and compressed with zstd. Just running `zstd` on the bytes will already give you a trivially parsable file. Designed to be as fast, and in some cases even faster, than [zlob](https://github.com/dmtrKovalenko/zlob). It comes close to the "OS speed of light", both on Windows and Linux.

**Experimental**: the format and the API can change without notice.

What makes it particularly notable is how it can keep max memory very low by practically never having to keep the entire index in memory. We also very heavily benchmarked and profiled it (on 8 core VMs with 16GB of RAM, with local SSDs). It is designed to also work well on remote filesystems or machines with hundreds of cores, but this has not been fully validated.

## Attribution

- [stringtape](https://crates.io/crates/stringtape): the string-tape layout that `src/tape.rs` uses. Its not revolutionary of course, we mostly borrow its name.
- Dmitriy Kovalenko's [zlob](https://github.com/dmtrKovalenko/zlob): this project after the original proof of concept but was used extensively as a benchmark target after https://github.com/borink-org/tarseer/pull/3 was merged. It's not fully equivalent, as we don't actually care that much about the globbing part and seek to do different things with it (as opposed to search). Similarly, BurntSushi's [ignore](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore) and [jwalk](https://github.com/Byron/jwalk) were also inspirations.

## LLM disclaimer

This project is heavily AI-assisted. It's based on a private proof of concept. That original proof of concept was then reconstructed in multiple steps (you can see the PRs and commits). Note that basically none of the individual lines of code and API docs are written by hand, but the models were heavily steered and went through multiple rounds of review. However, these reviews were not extremely detailed. It is very possible the APIs are stupid and the code could be much more readable and simple in places. Care was taken for the API docs to be nice to read and not contain _too_ much slop. Some stuff certainly slipped through review, we're all still new to this era of software engineering (I still think that, as opposed to just pure vibe coding, this still counts as engineering! Feel free to disagree).
