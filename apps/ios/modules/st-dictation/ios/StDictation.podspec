Pod::Spec.new do |s|
  s.name           = 'StDictation'
  s.version        = '0.1.0'
  s.summary        = 'On-device dictation for the Small Talk app'
  s.description    = 'Transcribes speech on the device with SpeechAnalyzer and reports words and levels.'
  s.author         = ''
  s.homepage       = 'https://github.com/compoundingtech/smalltalk'
  s.license        = 'MIT'
  s.platforms      = { :ios => '15.1' }
  s.source         = { git: '' }
  s.static_framework = true
  s.dependency 'ExpoModulesCore'
  s.source_files = '**/*.{h,m,swift}'
end
