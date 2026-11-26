require 'json'

package = JSON.parse(File.read(File.join(__dir__, '..', 'package.json')))

Pod::Spec.new do |s|
  s.name           = 'MasilLivePulse'
  s.version        = package['version']
  s.summary        = 'iOS ActivityKit Dynamic Island and Lock Screen Fleet Pulse for masil'
  s.description    = 'Opt-in minimal-metadata ActivityKit Live Activity bridge and token observer for masil mobile.'
  s.license        = package['license']
  s.author         = 'masil contributors'
  s.homepage       = 'https://github.com/masil-dev/masil'
  s.platforms      = { :ios => '16.4' }
  s.swift_version  = '5.9'
  s.source         = { :path => '.' }
  s.static_framework = true

  s.dependency 'ExpoModulesCore'
  s.weak_frameworks = 'ActivityKit', 'WidgetKit'

  s.source_files = 'MasilFleetAttributes.swift', 'MasilLivePulseModule.swift'
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'SWIFT_COMPILATION_MODE' => 'wholemodule'
  }
end
