Pod::Spec.new do |s|
  s.name           = 'StDeviceKey'
  s.version        = '0.1.0'
  s.summary        = 'The Small Talk app signing key'
  s.description    = 'Makes and keeps the P-256 key a paired phone signs its messages with.'
  s.author         = ''
  s.homepage       = 'https://github.com/compoundingtech/smalltalk'
  s.license        = 'MIT'
  s.platforms      = { :ios => '15.1' }
  s.source         = { git: '' }
  s.static_framework = true
  s.dependency 'ExpoModulesCore'
  s.source_files = '**/*.{h,m,swift}'
end
