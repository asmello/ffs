# ffs
Share files fast in a local trusted network, without encryption.


## Specification

See [PROTOCOL.md].

## FAQ

### Why not `scp`, `rsync`, `(s)ftp` or any number of well established solutions?

While those tools do solve the same problem (namely, file transfer), they come
with a number of downsides:
- `scp`, `rsync` (via `ssh`) and `sftp` all mandate encryption, which can
  sometimes be a bottleneck. In my Raspberry Pi 4, sending files over a gigabit
  link, I can only reach about 4MB/s at best, grossly under-utilising the
  network - and that's is entirely due to slow encryption. This can be mitigated
  by some fine-tuning of ciphers, but that's a pain to configure.
- `rsync` (via daemon) and `ftp` do work a lot better, but they are a pain to
  setup, especially outside Linux.

I wanted something that "just works" without any configuration, and was very
fast. I also don't need encryption inside my home network - if someone is
snooping I have much bigger problems than some cat videos being intercepted.
I've wanted this often enough over the years that I figured I may as well make
it happen.

