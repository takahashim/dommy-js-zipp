# frozen_string_literal: true

module ZippRb
  # Maps module names (URLs, bare names — anything a module loader answers)
  # onto the virtual paths the engine's module system works in, for
  # VM#module_loader=. Each module gets a directory of its own whose name
  # encodes the module name, so the referrer directory the engine hands the
  # resolver identifies the importing module. Internal.
  class ModulePaths
    ROOT = "/zipp-modules"
    FILE = "module.mjs"

    # Sources by path, read by the engine's module provider.
    attr_reader :sources

    def initialize
      @sources = {}
      @resolved = {}
    end

    # The path to load for +specifier+ imported from the module whose
    # directory is +dir+ (nil, or the root, outside any module). The engine
    # asks more than once per import; the loader is asked once.
    def resolve(loader, dir, specifier)
      key = [dir, specifier]
      return @resolved[key] if @resolved.key?(key)

      @resolved[key] = ask(loader, dir, specifier)
    end

    # The module name a path stands for (its `import.meta.url`).
    def name_of(path)
      dir = ::File.dirname(path)
      dir == ROOT ? nil : decode(::File.basename(dir))
    end

    private

    def ask(loader, dir, specifier)
      importer = dir && dir != ROOT ? decode(::File.basename(dir)) : nil
      answer = loader.call(specifier, importer)
      case answer
      when nil then unresolved_path(specifier)
      when String then store(specifier, answer)
      when Hash
        name = answer[:as] || answer["as"] || specifier
        code = answer.key?(:code) ? answer[:code] : answer["code"]
        code.nil? ? path_of(name) : store(name, code)
      else
        raise TypeError, "a module loader answers a String, a Hash or nil (got #{answer.class})"
      end
    end

    def store(name, code)
      path = path_of(name)
      @sources[path] = code.to_s
      path
    end

    def path_of(name) = "#{ROOT}/#{encode(name.to_s)}/#{FILE}"

    # A path no module is ever stored under (the file name differs), whose
    # directory still names the specifier, so "module not found" names it.
    def unresolved_path(specifier) = "#{ROOT}/#{encode(specifier.to_s)}/unresolved.mjs"

    # Percent-encode everything but [A-Za-z0-9_-], so the name is one path
    # component and never `.` or `..`.
    def encode(name)
      name.b.gsub(/[^A-Za-z0-9_-]/n) { |c| format("%%%02X", c.ord) }
    end

    def decode(component)
      component.b.gsub(/%([0-9A-F]{2})/n) { ::Regexp.last_match(1).hex.chr }.force_encoding(Encoding::UTF_8)
    end
  end
end
