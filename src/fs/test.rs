// SPDX-License-Identifier: GPL-2.0-or-later

use crate::{Fs, MapEntry, MapFs, dir_fs, error::FsError, test_fs::test_file_system};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[test]
fn test_map_fs() {
    let m: &dyn Fs = &MapFs(HashMap::from([
        (
            PathBuf::from("hello"),
            MapEntry {
                data: b"hello, world\n".to_vec(),
                is_file: true,
            },
        ),
        (
            PathBuf::from("fortune/k/ken.txt"),
            MapEntry {
                data: b"If a program is too slow, it must have a loop.\n".to_vec(),
                is_file: true,
            },
        ),
    ]));
    let want = &[PathBuf::from("hello"), PathBuf::from("fortune/k/ken.txt")];
    if let Err(e) = test_file_system(m, want) {
        println!("{e}");
        panic!("");
    }
}

// Non-UTF8 names must be rejected with an error, not a panic (valid_path
// previously unwrapped `name.to_str()`).
#[test]
fn test_open_non_utf8_name() {
    use crate::sub_fs::SubFs;
    use std::os::unix::ffi::OsStrExt;

    let name = Path::new(std::ffi::OsStr::from_bytes(b"\xff\xfe"));

    let map: &dyn Fs = &MapFs(HashMap::new());
    assert!(map.open(name).is_err());

    let dir = dir_fs(PathBuf::from("testdata"));
    assert!(dir.open(name).is_err());

    let sub: &dyn Fs = &SubFs {
        fsys: map.clone(),
        dir: PathBuf::from("sub"),
    };
    assert!(sub.open(name).is_err());
}

#[test]
fn test_dir_fs() {
    // On Windows, we force the MFT to update by reading the actual metadata from GetFileInformationByHandle and then
    // explicitly setting that. Otherwise it might get out of sync with FindFirstFile. See golang.org/issues/42637.
    /*#[cfg(target_os = "windows")]
    {
        /*if err := filepath.WalkDir("./testdata/dirfs", func(path string, d fs.DirEntry, err error) error {
            if err != nil {
                t.Fatal(err)
            }
            info, err := d.Info()
            if err != nil {
                t.Fatal(err)
            }
            stat, err := Stat(path) // This uses GetFileInformationByHandle internally.
            if err != nil {
                t.Fatal(err)
            }
            if stat.ModTime() == info.ModTime() {
                return nil
            }
            if err := Chtimes(path, stat.ModTime(), stat.ModTime()); err != nil {
                t.Log(err) // We only log, not die, in case the test directory is not writable.
            }
            return nil
        }); err != nil {
            t.Fatal(err)
        }*/
    }*/

    let mut test_files_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    test_files_path.push("testdata");
    test_files_path.push("dirfs");

    let fs = dir_fs(test_files_path);
    let want: Vec<_> = vec!["a", "b", "dir/x"]
        .into_iter()
        .map(PathBuf::from)
        .collect();

    if let Err(e) = test_file_system(&*fs.clone(), &want) {
        println!("{e}");
        panic!("");
    }

    // Test that the error message does not contain a backslash,
    // and does not contain the DirFS argument.
    let nonesuch = PathBuf::from("dir/nonesuch");
    //const nonesuch = "dir/nonesuch"
    match fs.open(&nonesuch) {
        Ok(_) => panic!("fs.open of nonexistent file succeeded"),
        Err(FsError::Io(_)) => {}
        Err(e) => panic!("{}", e),
    }
    /*_, err := fs.Open(nonesuch)
    if err == nil {
        t.Error("fs.Open of nonexistent file succeeded")
    } else {
        if !strings.Contains(err.Error(), nonesuch) {
            t.Errorf("error %q does not contain %q", err, nonesuch)
        }
        if strings.Contains(err.(*PathError).Path, "testdata") {
            t.Errorf("error %q contains %q", err, "testdata")
        }
    }*/

    // Test that Open does not accept backslash as separator.
    let d = dir_fs(PathBuf::from("."));
    assert!(
        d.open(Path::new("testdata\\dirfs")).is_err(),
        "open testdata\\dirfs succeeded"
    );

    // Test that Open does not open Windows device files.
    assert!(d.open(Path::new("NUL")).is_err(), "open NUL succeeded");
}

/*fn test_dir_fs_root_dir() {
    let cwd = current_dir().unwrap();

    //cwd = cwd[len(filepath.VolumeName(cwd)):] // trim volume prefix (C:) on Windows
    //cwd = filepath.ToSlash(cwd)               // convert \ to /
    //cwd = strings.TrimPrefix(cwd, "/")        // trim leading /

    // Test that Open can open a path starting at /.
    /*d := DirFS("/")
    f, err := d.Open(cwd + "/testdata/dirfs/a")
    if err != nil {
        t.Fatal(err)
    }
    f.Close()*/
}*/

/*
func TestDirFSEmptyDir(t *testing.T) {
    d := DirFS("")
    cwd, _ := os.Getwd()
    for _, path := range []string{
        "testdata/dirfs/a",                          // not DirFS(".")
        filepath.ToSlash(cwd) + "/testdata/dirfs/a", // not DirFS("/")
    } {
        _, err := d.Open(path)
        if err == nil {
            t.Fatalf(`DirFS("").Open(%q) succeeded`, path)
        }
    }
}

func TestDirFSPathsValid(t *testing.T) {
    if runtime.GOOS == "windows" {
        t.Skipf("skipping on Windows")
    }

    d := t.TempDir()
    if err := os.WriteFile(filepath.Join(d, "control.txt"), []byte(string("Hello, world!")), 0644); err != nil {
        t.Fatal(err)
    }
    if err := os.WriteFile(filepath.Join(d, `e:xperi\ment.txt`), []byte(string("Hello, colon and backslash!")), 0644); err != nil {
        t.Fatal(err)
    }

    fsys := os.DirFS(d)
    err := fs.WalkDir(fsys, ".", func(path string, e fs.DirEntry, err error) error {
        if fs.ValidPath(e.Name()) {
            t.Logf("%q ok", e.Name())
        } else {
            t.Errorf("%q INVALID", e.Name())
        }
        return nil
    })
    if err != nil {
        t.Fatal(err)
    }
}
*/
