Pod::Spec.new do |s|
  s.name = 'StAppUpdates'
  s.version = '0.1.0'
  s.summary = 'Signed paired-gateway app update transport'
  s.description = 'Changes the daily app download endpoint without disabling Expo recovery.'
  s.author = ''
  s.homepage = 'https://github.com/compoundingtech/smalltalk'
  s.license = 'MIT'
  s.platforms = { :ios => '16.4' }
  s.source = { git: '' }
  s.static_framework = true
  s.dependency 'ExpoModulesCore'
  s.dependency 'EXUpdates'
  s.source_files = '**/*.{h,m,swift}'
end
