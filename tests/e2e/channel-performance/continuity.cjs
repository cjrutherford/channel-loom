// Identity must come from persisted schedule/guide data, never inferred from PTS.
function checkContinuity(previous, current, jitterSeconds) {
  if (!Number.isFinite(jitterSeconds) || jitterSeconds < 0 || jitterSeconds > 1) throw new Error('jitter must be between 0 and 1 second');
  for (const sample of [previous, current]) {
    if (!sample.identity || !Number.isFinite(sample.sourceSeconds)) throw new Error('missing decoded marker or persisted broadcast identity');
  }
  if (!current.intentionalSeek && current.identity === previous.identity && current.sourceSeconds < previous.sourceSeconds - jitterSeconds) {
    return `content replay: ${previous.sourceSeconds} -> ${current.sourceSeconds} (${current.identity})`;
  }
  return null;
}
module.exports = { checkContinuity };
