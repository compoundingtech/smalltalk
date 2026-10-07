Pod::Spec.new do |s|
  s.name           = 'StDiagnostics'
  s.version        = '0.1.0'
  s.summary        = 'Bounded on-device Small Talk diagnostics'
  s.description    = 'Persists native launch breadcrumbs and sanitized MetricKit diagnostics before JavaScript starts.'
  s.author         = ''
  s.homepage       = 'https://github.com/compoundingtech/smalltalk'
  s.license        = 'MIT'
  s.platforms      = { :ios => '15.1' }
  s.source         = { git: '' }
  s.static_framework = true
  s.dependency 'ExpoModulesCore'
  s.frameworks = 'MetricKit', 'UIKit'
  s.source_files = '**/*.{h,m,swift}'
end
