# Security Policy

## Reporting a vulnerability

Report suspected vulnerabilities privately, not in a public issue.

Use GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository. If you cannot use it, contact the maintainers directly.

Please include the affected version, a description of the impact, and a minimal
reproduction if you have one. A volume image that reproduces a mount-time defect
is worth more than a description of one.

## Scope

The most useful reports are about the code here:

- **A malformed volume that is not rejected.** Every field the decoder reads has
  an implied bound. An image that makes the reader allocate, index, or loop past
  it is a finding, whether the outcome is a panic, a hang, or a silently wrong
  answer.
- **A crash window that loses or corrupts committed data.** The commit ordering
  in [`FORMAT.md`](FORMAT.md) is the claim; a fault injection that breaks it is a
  finding. Power loss at any point before the superblock write must leave the
  previous root intact and reachable.
- **Mount selection picking the wrong root.** A volume whose superblock copies
  disagree must resolve to one of them by the documented rule, never to a blend
  of several.
- **Refcount or reachability errors that free live data.** The allocator and the
  garbage collector both decide that a block is free; a case where either is
  wrong deletes a reachable object.
- **The encryption path.** `data-encryption` is off by default; when on, a nonce
  reuse, a key that survives unmount, or a plaintext block reaching the device
  are all findings.

Reports about the *design* are welcome as issues, not as vulnerabilities; see
[`KNOWN_ISSUES.md`](KNOWN_ISSUES.md), which lists the known limitations,
including the ones that are deliberate.

## What this is not

LedgerFS is pre-1.0 and the on-disk format is not frozen. A future format change
is not a vulnerability, and neither is the absence of a feature listed as
unimplemented in [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md).

There is no threat model in which an attacker who can write to the raw device
loses: an offline writer can replace any block, and the format's response is to
detect that and refuse, not to recover the data. Confidentiality of a
`data-encryption` volume rests on the key, and on the block device underneath it:
nothing here defends a volume against a device that lies.

## Supported versions

Security fixes land on `main`. There are no maintained release branches yet.
