#!/usr/bin/env bash

arch_normalize_source_tree() {
	local expected="${srcdir}/${pkgname}-${pkgver}"
	local root
	root="$(find "$srcdir" -mindepth 1 -maxdepth 1 -type d -print -quit)"
	[[ -n "$root" ]] || return 1
	if [[ "$root" != "$expected" ]]; then
		mv -- "$root" "$expected"
	fi
}

arch_check_payload() {
	local source_root="${srcdir}/${pkgname}-${pkgver}"
	test -x "$source_root/target/release/argvus-config"
	test -f "$source_root/src/usr/share/argvus/config/schema.json"
}

arch_package_payload() {
	local source_root="${srcdir}/${pkgname}-${pkgver}"
	install -Dm755 "$source_root/target/release/argvus-config" \
		"$pkgdir/usr/bin/argvus-config"
	install -Dm644 "$source_root/src/usr/share/argvus/config/schema.json" \
		"$pkgdir/usr/share/argvus/config/schema.json"
	install -Dm644 "$source_root/LICENSE" \
		"$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
