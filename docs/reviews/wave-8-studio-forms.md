# Wave 8: Studio form feedback

The language defaults, channel stream profile, Google Drive source and folder controls now show saving, success and failure status. Rejected field-specific responses receive inline guidance, `aria-invalid` and an `aria-describedby` association; the live status receives focus on failure. Inputs remain in place on failure, including credentials, and pending saves are guarded against duplicate requests. Successful source creation clears the credential. Library search has an accessible name. Server response text is not displayed: known validation errors map to fixed guidance and other failures use a safe retry message.

## Local evidence

- `cargo build --offline -p awesometv-server` passed (existing compiler warnings).
- The isolated fixture lifecycle in `scripts/run-isolated-e2e.py` was run via a temporary copy selecting `studio-forms.spec.ts` and passing its generated fixture password as `E2E_PASSWORD`; one Playwright Chromium test passed. The lifecycle generated and validated media, launched a disposable local server/database, authenticated, and cleaned up. It exercised keyboard submission and focus, real language, stream, Google credential and folder validation followed by corrected saves, retained values, accessible associations and search name. A route interception returned a delayed 503 to exercise loading, safe failure, retained credentials and the duplicate guard. That 503 is simulated; the validation and successful writes use the local server.
- Screenshot: `test-results/wave-8-studio-forms.png` (fixture data). The first run failed because the test targeted the embedded login form rather than the separate unauthenticated login page; the corrected run passed.

- Review follow-up: `timeout 180s cargo build --offline -p awesometv-server` passed with existing warnings. A temporary copy of `scripts/run-isolated-e2e.py` selected `studio-forms.spec.ts` and supplied its generated `E2E_PASSWORD`; `timeout 180s python3` running that copy passed 1 Chromium test and cleaned the server/data. In addition to the paths above, it submits an overlong channel audio language, checks focused guidance and retained input, then corrects and saves. Log: `/tmp/wave8-review-forms.log`.

## Limits

The channel stream API's `language codes are limited to 16 characters` error does not identify whether audio or subtitle language failed, so it cannot reliably be associated with one field. The form instead tells the user to check both audio and subtitle codes and limit each to 16 characters; it does not mark either field invalid on a guess. Unknown server errors are also deliberately general to avoid exposing submitted secrets. No server contract was changed. Hosted CI, real Google accounts/connectivity and assistive-technology manual testing were not exercised. Responsive Studio and broader form coverage remain separate work.
