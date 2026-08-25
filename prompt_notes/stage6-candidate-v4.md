# Stage 6 candidate v4 — intent-shaped

Supersedes the enumerated v3 in [stage6-security-scope.md](stage6-security-scope.md).
v3 was abandoned as a lookup table: each verification round surfaced one leak, the fix
was another clause, and the result was 8000 chars of enumerated issue types that a
competent agent can reframe against. The agents' own critiques said so — "decided by
the finding's framing rather than by the text", "depends on reading order".

The three duplicate categories in that note (consequence-severity substituted for
security consequence; provenance substituted for reachability; vocabulary borrowed
without mechanism) are one substitution: answering *what could go wrong here* instead
of *what does an adversary gain here*. v4 states that once and gives a subtractive
test, rather than enumerating the ways it can be got wrong.

**Not validated.** v3's numbers do not transfer. v4 has not been run against the
protected set of 145 sole-source findings, and its central risk is the opposite of
v3's: a one-sentence intent plus a subtractive test is more permissive to a motivated
reframer and harder to regression-test. Do not install on the strength of this note.

```
# Stage 6. Security audit

SCOPE: You audit ONLY security. Other pipeline agents cover high-level design (Stage 1), implementation completeness (Stage 2), control flow (Stage 3), resource lifecycle (Stage 4), locking/concurrency (Stage 5), and hardware (Stage 7). Each reports its own bugs and you should assume they will.

You are a Red Team security researcher auditing a Linux kernel patch. Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, time-of-check to time-of-use (TOCTOU) races, and information leaks (e.g., copying uninitialized kernel memory to user-space via copy_to_user). Scrutinize all points where untrusted user input reaches sensitive functions without validation. Ensure all length checks and bounds checks are robust against malicious input. Focus heavily on attack surfaces and data boundaries.

Your subject is what an adversary comes away holding that they should not: memory they can read, write, or reuse; data they should not see; access, privilege, or identity they were not granted; a check that was supposed to stop them and does not. Not how badly the kernel suffers. A patch that lets an unprivileged user panic the box is a serious bug and it is not yours — the adversary gains nothing they can build on, and the stage that owns the mechanism is already reporting it.

Apply this test to every concern before you keep it. Delete the adversary from your write-up and read what remains. If it is still a defect worth reporting — a leak, a deadlock, a missed error path, a device left misprogrammed — then it was always that stage's finding and the adversary was decoration. Keep it only if removing them leaves nothing to say, because what you are describing is the gain itself.

Where an untrusted value comes from matters far less than where it lands. That a length arrived from a peer, a device's firmware, or a lower-privileged function over a mailbox does not make a concern yours; that it reaches a copy, an index, an allocation, or an authorization decision without a bound does. Trace it to the landing site and say what happens there, or drop it.

When your evidence stops short, say where it stopped rather than reaching for a consequence you can prove. An adversarial gain you can name but not fully reach is worth reporting with the gap stated; a gain you cannot name is not a finding, and the bug you found while looking for it belongs to whoever owns it. Put those in dismissed_concerns and stop — do not re-file them under your own heading.
```

## What is preserved and why

The persona paragraph is byte-identical to the current prompt. Measurement in the v3
note found it load-bearing for all 145 sole-source findings, and the enumerated bug
list is the part that makes stage 6 look in the right places. v4 changes what stage 6
*keeps*, not what it looks for.

## What replaced what

| v3 mechanism | v4 |
|---|---|
| 5-bullet REPORT enumeration of outcomes | one sentence naming the four things an adversary can gain |
| 6-bullet HAND OFF list + precedence rule | the subtractive test |
| device/firmware/mailbox "is untrusted input too" | provenance-vs-landing-site paragraph, which inverts it |
| carve-out paragraph for missing bounds | falls out of the landing-site rule |
| closing filing rule | last paragraph, unchanged in intent |

The provenance inversion is the substantive reversal. v3 *added* "values supplied by a
device, its firmware, a DMA descriptor, or a peer ... are untrusted input too" to
protect the 40 sole-source findings whose surface is `device_hardware_input`. But
provenance-as-argument is the single most common marker among the 69 misplaced
findings (68%), so that sentence protects real findings by feeding the largest
duplicate category. v4 keeps the protection by pointing at the landing site instead:
the 40 device-input findings all trace to a copy, index, or allocation, and the
misplaced ones do not.

## What to test before installing

1. **The protected set.** All 37 strong and 73 moderate sole-source findings, same
   method as v3. The subtractive test is the risk: a bug can be genuinely stage 6's
   *and* still stand as a defect with the adversary removed — #7352 (ipset UAF) and
   #9432 (out-of-object write) are the shapes to check first.
2. **The 69 misplaced duplicates.** Whether one intent sentence suppresses what six
   enumerated bullets did.
3. **The four boundary cases v3 never settled** (#9703, #9604, #9697, #10055) — v4
   has no clause for them at all, by design. Under the subtractive test all four
   should hand off, since each stands as a lifecycle or state-reset defect without an
   adversary. That is the cheap check of whether intent beats enumeration here.
4. **Internal conflict, not just recall and suppression.** The two rules can pull
   against each other: for a device-supplied length reaching a `memcpy`, the
   subtractive test arguably leaves a missing-bounds defect standing (hand off) while
   the landing-site paragraph claims it (keep). Test that pair directly.
5. **Prompt budget.** 2131 chars vs the current 977 and v3's 8000.

## Why v3 was abandoned rather than patched further

Two independent judges scored v3's final separator 7/7 on verdicts, and both returned
`separates_cleanly: false` for different reasons. The second named the structural cause:
v3 stated precedence for HAND OFF over REPORT but never for HAND OFF vs. HAND OFF. With
six hand-off bullets that is fifteen unarbitrated pairs, three of them live (#10055, the
#9703 locking variant, #9604). Every fix adds a bullet and N more pairs — which is why
each verification round found exactly one more leak, and why the text reached 8000 chars.
An enumerated prompt generates conflicts faster than they can be closed. That is the
argument for stating intent once and testing subtractively, and it came from an agent
reading v3, not from the author of v4.
