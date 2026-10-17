# Security

gpuzip opens archives that may come from anyone, damaged by accident or crafted on purpose. It
refuses entries whose path would leave the output folder ("zip slip"), checks every CRC, and
its decoders are fuzzed against corrupt input; a hole in any of that is a security issue.

Please report one privately through GitHub's
[security advisories](https://github.com/andresayac/gpuzip/security/advisories/new) rather than
in a public issue, with the archive or a script that makes it, the command, and what happened.
