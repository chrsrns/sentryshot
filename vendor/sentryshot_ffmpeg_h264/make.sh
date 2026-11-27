#!/bin/sh

set -e

usage="Build Utilities
  Commands:
    $0 help           # Display this help and exit.
    $0 bindgen        # Generate 'bindings.rs' from 'wrapper.h'.
    $0 prepare-ffmpeg # Download and build ffmpeg versions.
    $0 test           # Run the tests on all ffmpeg versions.
    $0 test-valgrind  # Run the tests on all ffmpeg versions with valgrind.
"

# Go to project root.
script_path=$(readlink -f "$0")
script_dir=$(dirname "$script_path")
cd "$script_dir"
ffmpeg_dir="$script_dir"/ffmpeg

parse_command() {
	case $1 in
	bindgen)
		shift
		bindgen ./sentryshot_ffmpeg_h264_sys/src/wrapper.h \
			-o ./sentryshot_ffmpeg_h264_sys/src/bindings.rs \
			--raw-line '#![allow(
    unused,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    clippy::as_conversions,
    clippy::unreadable_literal,
    clippy::tests_outside_test_module
)]'

		exit 0
		;;
	prepare-ffmpeg)
		shift
		prepare_ffmpeg "6.0"
		prepare_ffmpeg "5.0.3"
		prepare_ffmpeg "4.1.11"
		exit 0
		;;
	test)
		shift
		cd ..
		test "6.0" "$@"
		test "5.0.3" "$@"
		test "4.1.11" "$@"
		exit 0
		;;
	test-valgrind)
		shift
		cd ..
		test_valgrind "6.0" "$@"
		test_valgrind "5.0.3" "$@"
		test_valgrind "4.1.11" "$@"
		exit 0
		;;
	esac
	printf "%s" "$usage"
}

prepare_ffmpeg() {
	version=$1

	mkdir -p "$ffmpeg_dir"/temp
	cd "$ffmpeg_dir"

	wget https://ffmpeg.org/releases/ffmpeg-"$version".tar.xz
	tar -xf ffmpeg-"$version".tar.xz -C ./temp --strip-components 1

	rm ffmpeg-"$version".tar.xz
	cd ./temp

	./configure \
		--prefix="$ffmpeg_dir"/"$version" \
		--enable-gpl \
		--enable-version3 \
		--disable-static \
		--enable-shared \
		--enable-debug=2 \
		--disable-all \
		--disable-programs \
		--disable-doc \
		--enable-avcodec \
		--enable-avutil \
		--enable-libx264 \
		--enable-decoder=h264

	make -j "$(nproc)"
	make install-libs install-headers

	cd ..
	rm -r ./temp
}

test() {
	version=$1
	shift
	pkg_config_path="$ffmpeg_dir"/"$version"/lib/pkgconfig
	ld_library_path="$ffmpeg_dir"/"$version"/lib
	
	if [ ! -d "$pkg_config_path" ]; then
		printf "%s does not exist\n" "$pkg_config_path"
		exit 1
	fi
	if [ ! -d "$ld_library_path" ]; then
		printf "%s does not exist\n" "$ld_library_path"
		exit 1
	fi

	export PKG_CONFIG_PATH="$pkg_config_path"
	export LD_LIBRARY_PATH="$ld_library_path"
	cargo test "$@"
}

test_valgrind() {
	version=$1
	shift
	printf "testing ffmpeg%s\n" "$version"
	pkg_config_path="$ffmpeg_dir"/"$version"/lib/pkgconfig
	ld_library_path="$ffmpeg_dir"/"$version"/lib
	
	if [ ! -d "$pkg_config_path" ]; then
		printf "%s does not exist\n" "$pkg_config_path"
		exit 1
	fi
	if [ ! -d "$ld_library_path" ]; then
		printf "%s does not exist\n" "$ld_library_path"
		exit 1
	fi

	export PKG_CONFIG_PATH="$pkg_config_path"
	export LD_LIBRARY_PATH="$ld_library_path"
	cargo valgrind test "$@"
}

parse_command "$@"
