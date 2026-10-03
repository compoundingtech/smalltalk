# Push notifications for a self-built Small Talk iOS app

Draft for review, 2026-10-02. Small Talk's iOS app is open source and not in the App Store: each person builds it with their own Apple team and bundle ID, and pairs it with their own st gateway. This is what it takes for that gateway to push to that phone, and how to coach someone through it. Claims not confirmed first-hand are marked (U) and listed under "Unverified" at the end; nothing here has been tried on a device yet.

## Summary

- A free Apple account (Personal Team) cannot use Push Notifications. A paid Apple Developer Program membership (USD 99/yr) is required for APNs.
- Each person needs their own bundle ID, team ID, and APNs auth key (.p8). The gateway signs ES256 JWTs with that key and talks HTTP/2 to APNs.
- An Xcode-installed, development-signed build gets a **sandbox** token (`api.sandbox.push.apple.com`) even if it is a Release build. An ad hoc build gets a **production** token. The app should report which one it has, and the gateway should fall back on `BadDeviceToken`.
- People without a paid account get no real push. Best fallback: ntfy or Pushover as a second, separate app.
- Repo note: `docs/st3/client-v0/README.md` says "v0 has no ... push notification service", and `docs/st3/app-design-session-brief.md` (line 215) leaves open which events deserve APNs. This research is input to that open question.

## 1. Apple account requirements

**Personal Team and push.** Xcode refuses the Push Notifications capability for free teams ("Personal development teams ... do not support the Push Notifications capability"). A forum participant quotes the wording from a failed build ([Apple forums 99936](https://developer.apple.com/forums/thread/99936)). The widely repeated summary is that push is supported for the paid Developer Program and Enterprise Program but not for "Apple Developer" (free provisioning). I could not extract Apple's own capability table (the page is JS-rendered): [Supported capabilities (iOS)](https://developer.apple.com/help/account/reference/supported-capabilities-ios/). (U: confirm in a browser.)

**What free gets you.** On-device testing in Xcode only. Certificates, Identifiers & Profiles, App Store Connect, and TestFlight are paid-only ([Compare memberships](https://developer.apple.com/support/compare-memberships/)). The key and App ID pages you need for APNs live in the paid portal.

**Expiry.** An Apple DTS engineer states that free (Personal Team) profiles expire after 7 days and paid profiles after a year ([forums 712540](https://developer.apple.com/forums/thread/712540)). Forum summaries also cite free-tier limits of about 10 App IDs per week, 3 devices, and 3 apps per device ([forums 786762](https://developer.apple.com/forums/thread/786762)). (U: these numbers are secondary.) Ad hoc profiles are a paid feature and last up to a year as well.

**Device limit.** 100 devices per product family (iPhone, iPad, ...) per membership year. Disabling a device does not free a slot; you can reset the list at renewal ([Devices overview](https://developer.apple.com/help/account/devices/devices-overview)). For one person with a phone or two this is irrelevant.

**Time Sensitive.** Sending `interruption-level: time-sensitive` also requires the Time Sensitive Notifications capability (entitlement `com.apple.developer.usernotifications.time-sensitive`) in the app ([WWDC21 10091 notes](https://www.wwdcnotes.com/notes/wwdc21/10091); [Pushwoosh how-to](https://help.pushwoosh.com/hc/en-us/articles/27979836066717-How-do-I-configure-and-use-Time-Sensitive-notifications-for-my-iOS-app)). It is a normal capability for paid teams. (U: Apple's own page on it did not load.) Without it Apple treats the level as `active`. Recommend: v1 uses `active` for everything; offer time-sensitive as an opt-in later.

## 2. Per-builder bundle ID and team

Bundle IDs are globally unique per team, so two people cannot both use `com.compoundingtech.smalltalk.starter`. Today `app.json` hard-codes it, and the README's deep-link test URLs use it as the URL scheme.

Expo's dynamic config lets `app.config.js` receive the static `app.json` and override it, with `process.env` available ([Expo configuration](https://docs.expo.dev/workflow/configuration/)). The relevant properties are `ios.bundleIdentifier` and `ios.appleTeamId` ("The Apple development team ID to use for all native targets"), plus `ios.entitlements` ([app.json reference](https://docs.expo.dev/versions/latest/config/app/)). Sketch:

```js
// apps/ios/app.config.js
module.exports = ({ config }) => {
  const bundleId = process.env.ST_IOS_BUNDLE_ID
    || (process.env.ST_IOS_BUNDLE_SUFFIX
        ? `${config.ios.bundleIdentifier}.${process.env.ST_IOS_BUNDLE_SUFFIX}`
        : config.ios.bundleIdentifier);
  return {
    ...config,
    ios: {
      ...config.ios,
      bundleIdentifier: bundleId,
      ...(process.env.ST_IOS_TEAM_ID ? { appleTeamId: process.env.ST_IOS_TEAM_ID } : {}),
    },
  };
};
```

Recommendations:

- Prefer a full `ST_IOS_BUNDLE_ID` override (e.g. `dev.alice.smalltalk`); it is simplest to explain. The suffix form is a convenience for people on the same team.
- Set `appleTeamId` from `ST_IOS_TEAM_ID`. `expo prebuild --clean` regenerates `ios/`, so a team picked by hand in Xcode is lost on every clean prebuild. With `appleTeamId` set the generated project already has `DEVELOPMENT_TEAM`.
- Keep the default (no env vars) identical to today so CI and maintainers are unaffected.
- The README's `com.compoundingtech.smalltalk.starter://...` links must become "your bundle ID" links. Add an explicit `scheme` (e.g. `smalltalk`) so links stop depending on the bundle ID. (U: check whether prebuild's default scheme is the bundle ID in this project.)
- The gateway must never hard-code the topic; the app sends its own bundle ID at registration (section 4).

## 3. APNs setup the person does

1. Sign in at developer.apple.com, Certificates, Identifiers & Profiles, Keys, create a key with the Apple Push Notifications service (APNs) box checked, and download the `.p8` once. Note the 10-character **Key ID** and the 10-character **Team ID** (Membership page). Apple: "Secure both pieces of information carefully" ([Token-based connection](https://developer.apple.com/documentation/usernotifications/establishing-a-token-based-connection-to-apns)).
2. One key works for all the person's apps: "You can use the same token from multiple provider servers" and "one token to distribute notifications for all or a subset of your company's apps" (same page).
3. **Key scope has changed.** Apple now offers team-scoped keys restricted to Sandbox or Production, with at most two keys per environment, and topic-specific keys. Older keys that work in both environments keep working, but Apple recommends environment-specific keys (same page). Coach people to create a key that works for **both** environments if the portal offers it, or two keys (sandbox and production) if they will ever use both a development and an ad hoc build. (U: exact portal UI wording.) A wrong-environment key shows up as `BadEnvironmentKeyInToken` ([forum 803458](https://developer.apple.com/forums/thread/803458); (U) I did not read this thread in full).
4. Give the gateway: `.p8` path, key ID, team ID, bundle ID (a topic), and default environment. Keep the `.p8` out of git and file mode 0600.

**Which endpoint.** The environment of a token is set by the app's `aps-environment` entitlement, which "Xcode sets ... based on your app's current provisioning profile" ([aps-environment](https://developer.apple.com/documentation/bundleresources/entitlements/aps-environment)). An Apple engineer: "Development or Production tokens can only be used in the environments they belong in. The environment a token belongs to is determined by the aps-environment entitlement" ([forum 689857](https://developer.apple.com/forums/thread/689857)).

- Installed from Xcode with an Apple Development profile (Debug or Release): `development`, so `api.sandbox.push.apple.com`. Expo's doc claims Xcode switches to production "during release builds" ([expo-notifications](https://docs.expo.dev/versions/v57.0.0/sdk/notifications/)), but per Apple the value follows the signing profile, so a Release scheme with development signing (what this README describes) stays development. (U: confirm on a real device; this is my inference.)
- Ad hoc, App Store, TestFlight: `production`, so `api.push.apple.com`.

**How the gateway knows.** Best: the app reports it. `expo-application`'s `getIosPushNotificationServiceEnvironmentAsync()` returns `'development'` or `'production'` (`null` on simulators), derived from the target's `aps-environment` ([docs](https://docs.expo.dev/versions/latest/sdk/application/)). Store `{token, environment, bundleId}` per device. Safety net: if APNs returns `BadDeviceToken` ("Verify that ... the token matches the environment", [response codes](https://developer.apple.com/documentation/usernotifications/handling-notification-responses-from-apns)), retry once on the other host and remember the winner. That fallback needs no app support.

## 4. What the gateway sends

Per [Sending notification requests to APNs](https://developer.apple.com/documentation/usernotifications/sending-notification-requests-to-apns):

- HTTP/2 and TLS 1.2+ to `api.push.apple.com:443` or `api.sandbox.push.apple.com:443`. Port 2197 is also allowed. Keep connections open and reuse them; a2's README warns that new connections per request can be treated as a DoS.
- `POST /3/device/<hex token>`.
- Headers: `authorization: bearer <JWT>`, `apns-topic: <bundle id>`, `apns-push-type: alert` (recommended for iOS; send it every time), `apns-priority: 10` (immediate; the default) or `5` (power-aware), optional `apns-collapse-id` (max 64 bytes, merges repeats), optional `apns-expiration` (0 = try once, no storage).
- Payload: JSON, uncompressed, max 4096 bytes ([payload doc](https://developer.apple.com/documentation/usernotifications/generating-a-remote-notification)). Put custom keys next to `aps`, not inside it. Keys: `alert{title,body}`, `badge`, `sound`, `thread-id`, `interruption-level`, `category`.
- JWT: header `{alg: ES256, kid: <key id>}`, claims `{iss: <team id>, iat: <now>}`. Refresh "no more than once every 20 minutes and no less than once every 60 minutes"; older than an hour gives `ExpiredProviderToken`; a new token more than once per 20 minutes on a connection gives an error (token-based connection page). Regenerate every ~40 minutes.
- Errors to handle: `BadDeviceToken` (environment or bad token), `DeviceTokenNotForTopic` and `BadTopic` (bundle ID mismatch), `Unregistered` (410; delete the token), `InvalidProviderToken` (wrong key, team, or key revoked), `TopicDisallowed`, `PayloadTooLarge`.
- Privacy and design: payloads pass through Apple. Send only a short, generic alert ("agent is waiting on you") plus an opaque item ID in a custom key. The app then fetches detail over its normal authenticated gateway connection. This also matches the README rule that the app holds no offline state.
- Use `collapse-id` per attention item so repeats merge. For badge, send the current unresolved attention count (the app's Home tab already computes it) and have the gateway own that number.

**Rust.** The `a2` crate (hyper/h2, ES256 `.p8` token auth with automatic renewal, `.p12` certs, MIT) is the usual choice: [crates.io a2](https://crates.io/crates/a2). Maintenance: latest release 0.10.0 on 2024-05-05; the repo moved to `reown-com/a2`, is not archived, and last had a push in 2026-01 with ~23 open issues (my query of the GitHub and crates.io APIs on 2026-10-02). It pins `rustls 0.22` and `hyper 1`, while this workspace's `Cargo.lock` has `rustls 0.23` and `reqwest 0.12`, so it would add a second TLS stack. Alternatives: `apns-h2` 0.11.0 (2026-02-09, a maintained a2 derivative, higher recent downloads; (U) I did not read its code) or about 100 lines on the existing hyper/reqwest stack plus the `p256`/`ring` ES256 signer. Recommendation: evaluate `apns-h2` first, and consider hand-rolling because the surface (one POST, one JWT) is tiny and the dependency churn costs more than the code.

## 5. Expo side

- `getDevicePushTokenAsync()` "returns a native FCM or APNs token" and is what you use when not using Expo's push service ([expo-notifications](https://docs.expo.dev/versions/v57.0.0/sdk/notifications/), [custom sending](https://docs.expo.dev/push-notifications/sending-notifications-custom/)). It needs no Expo account, EAS, or Expo push servers; only `getExpoPushTokenAsync` talks to Expo. This fits the README's "no Expo account, EAS service" path. (U: I did not execute it on this repo's build; the app has no `expo-notifications` dependency yet, `package.json` lists only build-properties, crypto, font, secure-store.)
- Config plugin: add `"expo-notifications"` to `plugins`. It "automatically sets the `aps-environment` entitlement to `'development'`" (same docs page); local `expo prebuild` writes it into the `.entitlements` file. Optional `enableBackgroundRemoteNotifications` adds `remote-notification` background mode; not needed for visible alerts. Alternative: `ios.entitlements: {"aps-environment": "development"}` ([custom sending](https://docs.expo.dev/push-notifications/sending-notifications-custom/)).
- Signing must also carry the capability. With automatic signing, Xcode registers the App ID with Push Notifications when the entitlement is present, and fails with the Personal Team message otherwise.
- Also add `expo-application` for the environment value (section 3). Re-register on every launch (Apple: "Never cache device tokens in local storage", [registering](https://developer.apple.com/documentation/usernotifications/registering-your-app-with-apns)). Send token, environment, bundle ID, and app version to the paired gateway over the existing authenticated socket, scoped like any other device capability.
- Permission: call `requestPermissionsAsync()` after pairing, not at cold start; a denied prompt cannot be re-shown, only changed in Settings.
- Taps and deep links: `addNotificationResponseReceivedListener` for taps while running, and `useLastNotificationResponse()` / `getLastNotificationResponse()` for cold start ([docs](https://docs.expo.dev/versions/v57.0.0/sdk/notifications/)). Read your custom key (the item ID) from the response and navigate to the Home attention item. (U: exact path to the custom payload in the response object differs for remote iOS notifications; check against the SDK 57 types.) Foreground display needs `setNotificationHandler`.
- Simulators on Xcode 14+ can receive pushes via `xcrun simctl push` (no real APNs), useful for testing taps without a paid account (docs page above).

## 6. Fallbacks for a free Apple ID

| Option | Instant? | Cost | Notes |
| --- | --- | --- | --- |
| BGTaskScheduler + local notifications | No | Free | `earliestBeginDate` only means "won't begin sooner"; the system gives no timing guarantee ([docs](https://developer.apple.com/documentation/backgroundtasks/bgtaskrequest/earliestbegindate)). Also a 7-day app expiry. Good only as a "check on wake" nicety. |
| Keep app foregrounded | Yes, while open | Free | The app already holds a WebSocket only in the foreground. Not push. |
| ntfy (second app) | Yes, if upstream is set | Free | iOS cannot hold a background socket, so a self-hosted server must set `upstream-base-url: "https://ntfy.sh"`; it forwards only a poll request (message ID plus topic hash) and the ntfy app fetches the body from you. Without it delivery "can take hours" ([ntfy config](https://docs.ntfy.sh/config/#ios-instant-notifications), [known issues](https://docs.ntfy.sh/known-issues/)). Verified in docs: still depends on ntfy.sh's APNs path. Easiest for a self-hoster; the gateway just POSTs to a topic. |
| Pushover | Yes | USD 4.99 per platform, 30-day trial, 10,000 msgs/mo free ([pricing](https://pushover.net/pricing)) | Hosted third party; message contents pass through Pushover. Reliable. |
| Email / iMessage / SMS | Yes | Free to varied | Email is simple but slow and noisy. iMessage has no official API and needs a Mac or bridge; (U) not researched. |

Suggested stance: generic "notifier" hook in the gateway with two backends, APNs (paid) and ntfy (free). Tapping an ntfy notification can open `<scheme>://...` links, reusing the app's deep-link handler (note the README says several links are disabled in Release).

## 7. Coaching checklist (paid account)

1. [ ] Confirm Apple Developer Program membership is active (not just an Apple ID). Failure: "does not support the Push Notifications capability" means Personal Team is selected.
2. [ ] Clone the repo; `cd apps/ios`; `npm ci`.
3. [ ] Pick a unique bundle ID, e.g. `dev.<you>.smalltalk`. Export `ST_IOS_BUNDLE_ID` and `ST_IOS_TEAM_ID` (Team ID from Membership). Failure: "bundle ID not available" = someone already owns it.
4. [ ] `npx expo prebuild --platform ios --clean --no-install`; `npm run pods`.
5. [ ] Open `ios/smalltalk.xcworkspace`; Signing & Capabilities; confirm Team is the paid team, Automatic signing on, Push Notifications capability present. Failure: wrong team, profile missing `aps-environment`.
6. [ ] Register the phone in Xcode (plug in, trust, enable Developer Mode on iOS 16+). Build and run on the device with development signing.
7. [ ] Create an APNs key in Certificates, Identifiers & Profiles, Keys. Save the `.p8`, Key ID, and Team ID. Failure: key limit reached (two per environment), or the `.p8` was lost (cannot be re-downloaded).
8. [ ] Put key path, key ID, team ID, bundle ID into the gateway's config. Restart the gateway.
9. [ ] Pair the app with the gateway; in the app allow notifications when asked. Failure: "Don't Allow" tapped; fix in iOS Settings, Notifications.
10. [ ] Confirm the gateway recorded `{token, environment: development, bundleId}` for the device.
11. [ ] Trigger a test push (agent waiting on you; or a gateway "send test push" command). Expect `200` from `api.sandbox.push.apple.com`.
12. [ ] Tap it; the app should open on the right Home item (warm and cold start).

Likely failures:

- `BadDeviceToken`: environment mismatch (ad hoc or TestFlight token sent to sandbox, or the reverse), or a token from a different app or reinstall.
- `DeviceTokenNotForTopic` / `BadTopic`: gateway's topic differs from the app's bundle ID.
- `InvalidProviderToken` / `ExpiredProviderToken`: wrong key ID or team ID, key revoked, or JWT older than an hour or clock skew on the server.
- `BadEnvironmentKeyInToken`: key restricted to the other environment.
- `TooManyProviderTokenUpdates`: JWT rebuilt more than every 20 minutes.
- Nothing arrives and no error: notifications denied, Focus mode, or the gateway has no network route to APNs (443 or 2197 outbound).
- Reinstall: new token, so the app must re-register on launch.
- 1-year expiry: development profiles last a year; re-run Xcode before then.

## Unverified

- Apple's iOS capability table (Personal Team vs paid for Push and Time Sensitive); relied on Apple forum threads. The Apple help pages did not render.
- Free-tier counts (10 App IDs, 3 devices, 3 apps) from forum summaries.
- Release-with-development-signing keeps `aps-environment=development` (my reading of Apple's description, contradicting Expo's looser wording).
- Exact shape of remote-notification payload in Expo's response object, and prebuild's default URL scheme in this project.
- Portal wording for environment-scoped keys; `apns-h2` quality; iMessage options.
- No real-device push was sent as part of this research.
