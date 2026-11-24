require 'json'

package = JSON.parse(File.read(File.join(__dir__, '..', 'package.json')))

Pod::Spec.new do |s|
  s.name           = 'MasilTerminal'
  s.version        = package['version']
  s.summary        = 'Native CoreText terminal canvas view for masil mobile'
  s.description    = 'Native CoreText terminal canvas view for masil mobile'
  s.license        = package['license']
  s.author         = 'masil contributors'
  s.homepage       = 'https://github.com/masil-dev/masil'
  s.platforms      = { :ios => '16.4' }
  s.swift_version  = '5.9'
  s.source         = { :path => '.' }
  s.static_framework = true

  s.dependency 'ExpoModulesCore'

  s.source_files = '**/*.{h,m,swift}'
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'SWIFT_COMPILATION_MODE' => 'wholemodule'
  }
end
