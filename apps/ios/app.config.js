const base = require('./app.json').expo;

// APP_VARIANT selects the installed identity. dev (default) is the Debug/Metro starter app;
// daily is the Release app used every day. Both coexist on one phone with separate Keychain profiles.
// The home-screen name is set through CFBundleDisplayName so Expo's `name`, and with it the
// generated Xcode project, workspace and scheme (`ios/smalltalk.xcworkspace`), stay the same for both.
module.exports = () => {
  const variant = process.env.APP_VARIANT ?? 'dev';
  if (variant !== 'dev' && variant !== 'daily') throw new Error('APP_VARIANT must be dev or daily');
  const daily = variant === 'daily';
  return {
    ...base,
    icon: daily ? './assets/icon.png' : './assets/icon-dev.png',
    ios: {
      ...base.ios,
      bundleIdentifier: daily ? 'com.compoundingtech.smalltalk' : 'com.compoundingtech.smalltalk.starter',
      infoPlist: { ...base.ios.infoPlist, CFBundleDisplayName: daily ? 'Smalltalk' : 'Smalltalk Dev' },
    },
  };
};
