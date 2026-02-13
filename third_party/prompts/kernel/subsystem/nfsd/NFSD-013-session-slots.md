# NFSD-013: Session slot state and SEQUENCE operations

**Risk**: Reply cache corruption, protocol violation, crash

## Check

- Session slot state properly maintained across SEQUENCE calls
- SEQUENCE reply caching follows RFC 8881 section 18.46
- Slot seqid validation prevents replay attacks
- Proper slot cleanup on session destruction
- DRC (duplicate request cache) size limits enforced
- Retry logic handles cached replies correctly

## Review focus

- Verify slot table allocation and bounds checking
- Check SEQUENCE operation validates slot numbers and seqids
- Ensure reply cache entries are properly referenced
- Validate slot state transitions (free→inuse→cached)
- Review memory management for cached replies
- Check concurrent access to slot state is properly locked

## Example vulnerability

SEQUENCE reply incorrectly cached (48990a0923a7)
