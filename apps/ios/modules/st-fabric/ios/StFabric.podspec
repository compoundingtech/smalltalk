Pod::Spec.new do |s|
  s.name = 'StFabric'
  s.version = '0.1.0'
  s.summary = 'Development-only fabric gateway carrier'
  s.description = 'An app-owned iroh identity and loopback byte bridge for a transport proof.'
  s.author = ''
  s.homepage = 'https://github.com/compoundingtech/smalltalk'
  s.license = 'MIT'
  s.platforms = { :ios => '16.4' }
  s.source = { git: '' }
  s.static_framework = true
  s.dependency 'ExpoModulesCore'
  if ENV['ST3_FABRIC_PROOF'] == '1'
    s.source_files = 'StFabricModule.swift'
    s.vendored_frameworks = 'build/StFabricRust.xcframework'
    s.frameworks = 'Security', 'SystemConfiguration', 'Network'
    s.libraries = 'resolv'
  else
    s.source_files = 'StFabricDisabled.swift'
  end
end
