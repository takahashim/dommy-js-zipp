# frozen_string_literal: true

source "https://rubygems.org"

# Every gem of this monorepo resolves from its local path, so a change in one
# is seen by the others without a release.
gem "zipp-rb", path: "gems/zipp-rb"
gem "dommy-js-zipp", path: "gems/dommy-js-zipp"

# dommy-js-zipp tracks dommy's main (the bridge's wire protocol runs ahead of
# the released gem). A sibling checkout of the dommy monorepo is used when
# present; otherwise dommy's main on GitHub.
dommy_gems = File.expand_path("../dommy/gems", __dir__)
if File.directory?(dommy_gems)
  gem "dommy", path: "#{dommy_gems}/dommy"
else
  git "https://github.com/takahashim/dommy.git", branch: "main", glob: "gems/*/*.gemspec" do
    gem "dommy"
  end
end

# Shared dev tooling.
gem "minitest"
gem "rake"
gem "rake-compiler"
