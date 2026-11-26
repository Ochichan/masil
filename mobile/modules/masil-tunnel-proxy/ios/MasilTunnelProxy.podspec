require 'json'

package = JSON.parse(File.read(File.join(__dir__, '..', 'package.json')))

Pod::Spec.new do |s|
  s.name           = 'MasilTunnelProxy'
  s.version        = package['version']
  s.summary        = 'Local loopback TCP proxy bridging iOS WebView traffic over Masil E2EE tunnel frames'
  s.description    = 'Runs a phone-local 127.0.0.1 NWListener that multiplexes HTTP and WebSocket streams into Noise NK binary tunnel frames.'
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
