# Fscrypt Subsystem Details

## Key Management
- Master keys in keyring, per-file keys derived
- Key removal makes files unreadable
- Keys must be zeroized after use
- Key identifiers are cryptographic hashes

## Encryption Context
- Stored in xattr or superblock
- Children inherit parent's encryption policy
- Cannot change after creation
- Contains policy version and encryption modes

## IV (Initialization Vector) Requirements
- Must be unique per file and logical block
- Different modes have different IV schemes
- IV reuse breaks confidentiality
- Per-file key derivation prevents IV collisions

## Quick Checks
- Encryption context set before data operations
- Proper padding to block size (usually 16 bytes)
- No plaintext filenames in encrypted directories
- Key availability before decrypt operations
