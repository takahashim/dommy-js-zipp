# dommy-js-zipp

JavaScript in Ruby on [Zipp](https://github.com/f2i-com/zipp.org), a
JavaScript engine written from scratch in Rust.

| Gem | What it is |
| --- | --- |
| [`zipp-rb`](gems/zipp-rb) | The Ruby binding: `ZippRb::VM` over the Zipp engine (Magnus / rb-sys native extension). |
| [`dommy-js-zipp`](gems/dommy-js-zipp) | A [Dommy](https://github.com/takahashim/dommy) JavaScript backend on zipp-rb, the counterpart of dommy-js-quickjs. |

```sh
bundle install
bundle exec rake    # compile and test every gem
```

Development notes for contributors and coding agents are in [AGENTS.md](AGENTS.md).
