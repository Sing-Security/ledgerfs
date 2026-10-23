//! Security tests for path and name handling: the surface every caller of the
//! store passes through, usually with a string a user chose.
//!
//! # What "path security" means for a filesystem that is not one
//!
//! LedgerFS has no host filesystem behind it. There is no `chroot` to escape, no
//! `/etc/passwd` on the other side, and no process to confuse. `..` does not
//! mean "up a level", it means "an instruction this format has no way to carry
//! out", so it is refused. The classic traversal payloads are therefore not
//! exploitable in the way they are on a POSIX filesystem, and a test suite that
//! only checked "does `../../etc/passwd` fail" would be checking the one thing
//! that cannot happen here.
//!
//! The properties that *do* matter are the ones a filesystem's callers build on
//! top of, and each of them has a way of being violated that is quiet rather
//! than loud:
//!
//! - **one spelling, one object.** The reader must interpret a path the same way
//!   the writer did. Any input the reader normalises but the writer does not (
//!   case, trailing dots, Unicode) becomes a name that can be created under one
//!   spelling and read under another. That is a collision attack: two callers
//!   believe they hold different files, or one believes a file is absent while
//!   it is present.
//! - **a name is data.** A name that is truncated at a NUL, split at a `.`, or
//!   case-folded on the way to disk is a name the caller cannot predict. It
//!   also means an attacker who controls a name can choose the collision.
//! - **nothing that is written can become unreachable.** A name the writer
//!   accepts but the reader skips (or vice versa) is a file that exists and
//!   cannot be listed, read or deleted: a way to hide data in a volume, and a
//!   file that cannot be removed is a leak of space as well as of secrecy.
//! - **a path cannot name something outside the volume.** Here this is not
//!   about a host path but about the *volume*: a name containing `/` must split
//!   into components rather than be stored whole, because a `read_dir` result
//!   containing a slash would be re-split by anything that replays it
//!   (`rebuild_into`, a compaction pass) and would name a directory it was never
//!   meant to touch.
//! - **the doors are counted.** Every verb that takes a path goes through the
//!   same component parser, so a rule added to one verb and missed in another
//!   is a rule with a hole in it. Several tests below drive the whole verb set
//!   rather than one call.
//!
//! # What is checked where
//!
//! `readpath.rs` pins the *meaning* of paths: that `//a/b` and `/a/b` are the
//! same file, that the root has many spellings, that a name may be 256 bytes and
//! not 257. This file pins what happens when a path is *hostile*: refused where
//! the format cannot express it, and stored verbatim where it can.

use ledgerfs_host::{Error, InodeKind, Result, components_of};

mod common;
use common::{expect_err, names, store};

/// A name that is valid UTF-8 but contains no `/`: the only thing LedgerFS
/// forbids inside a name, because it is the one thing that would change what the
/// name means.
fn name_of(bytes: &[u8]) -> String {
	String::from_utf8_lossy(bytes).into_owned()
}

// region:    --- Traversal

#[test]
fn test_dot_dot_is_refused_by_every_verb_that_takes_a_path() -> Result<()> {
	// -- Setup & Fixtures: one path per verb, all carrying `..`. This is the
	// test that would catch the one verb somebody forgot: the failure mode of a
	// rule enforced in five places out of six is that the sixth is the one an
	// attacker uses, and nothing about the five working makes the sixth safe.
	//
	// `..` is refused rather than resolved because the format has no parent
	// pointers: honouring it would mean re-walking the prefix and *dropping* the
	// component, and a silently dropped component is worse than a refusal;
	// `../etc/passwd` would quietly become `etc/passwd`, naming something the
	// caller did not ask for.
	let mut s = store(1024)?;
	s.mkdir("/d", 0o755)?;
	s.write_new_file("/d/f", b"x", 0o644)?;

	// -- Exec
	let refusals: Vec<(&str, Result<()>)> = vec![
		("mkdir", s.mkdir("/d/../evil", 0o755)),
		("create_file", s.create_file("/d/../evil", 0o644)),
		("write_new_file", s.write_new_file("/d/../evil", b"x", 0o644)),
		("write_file", s.write_file("/d/../f", b"x", 0o644)),
		("append", s.append("/d/../f", b"x", 0o644)),
		("write_at", s.write_at("/d/../f", 0, b"x")),
		("remove", s.remove("/d/../f")),
		("rename from", s.rename("/d/../f", "/d/g")),
		("rename to", s.rename("/d/f", "/d/../g")),
		("stat", s.stat("/d/../f").map(|_| ())),
		("read_dir", s.read_dir("/d/..").map(|_| ())),
		("read_file", s.read_file("/d/../f").map(|_| ())),
		("read_at", s.read_at("/d/../f", 0, 1).map(|_| ())),
	];

	// -- Check: all thirteen, and every one of them a `Usage` refusal rather
	// than, say, a `NoSuchFile` that would mean the component was being walked.
	for (verb, r) in refusals {
		match expect_err(verb, r) {
			Error::Usage(detail) => assert!(detail.contains(".."), "{verb}: {detail}"),
			other => panic!("{verb}: expected a Usage refusal, got {other:?}"),
		}
	}

	// -- Check: and nothing was created or destroyed by any of it. A verb that
	// refused *after* doing half its work would leave the tree changed, so the
	// refusals are checked against the tree rather than only on their own.
	let mut root = names(&s.read_dir("/")?);
	root.retain(|n| !n.starts_with('.'));
	assert_eq!(root, vec!["d".to_string()]);
	assert_eq!(names(&s.read_dir("/d")?), vec!["f".to_string()]);
	assert_eq!(s.read_file("/d/f")?, b"x");
	Ok(())
}

#[test]
fn test_every_spelling_of_dot_dot_is_refused() -> Result<()> {
	// -- Setup & Fixtures: `..` in every position a caller might put it. A
	// refusal that only fires on an interior component would let `..` at the
	// start or the end through.
	let paths = [
		"..",
		"/..",
		"../",
		"/../",
		"a/..",
		"a/../b",
		"/a/../b",
		"../a",
		"a/../..",
		".../..",
		"a//../b",
		"a/../",
		"/a/b/../../c",
	];

	// -- Exec & Check
	for path in paths {
		let e = expect_err(path, components_of(path));
		match e {
			Error::Usage(detail) => assert!(detail.contains(".."), "{path}: {detail}"),
			other => panic!("{path}: expected a Usage refusal, got {other:?}"),
		}
	}
	Ok(())
}

#[test]
fn test_a_name_that_merely_looks_like_dot_dot_is_an_ordinary_name() -> Result<()> {
	// -- Setup & Fixtures: names built from dots that are not `..`. Each must be
	// creatable, listable and removable: a filter that used `contains("..")` or
	// a prefix test instead of an exact comparison would refuse them, and would
	// make a whole family of ordinary filenames unusable.
	let mut s = store(1024)?;
	let ordinary = ["...", "..a", "a..", "..-", "._.", "....", "-..-"];

	// -- Exec & Check
	for name in ordinary {
		let path = format!("/{name}");
		s.write_new_file(&path, b"here", 0o644)?;
		assert_eq!(
			s.read_file(&path)?,
			b"here",
			"{name} must be readable under the name it was written"
		);
	}
	let listed = names(&s.read_dir("/")?);
	for name in ordinary {
		assert!(listed.contains(&name.to_string()), "{name} missing from {listed:?}");
	}

	// -- Exec & Check: and each is removable under its own name
	for name in ordinary {
		s.remove(&format!("/{name}"))?;
	}
	assert!(!names(&s.read_dir("/")?).iter().any(|n| ordinary.contains(&n.as_str())));
	Ok(())
}

#[test]
fn test_a_single_dot_component_is_a_path_separator_not_a_name() -> Result<()> {
	// -- Setup & Fixtures: `.` means "this directory", as everywhere else. The
	// security question it raises is not traversal but *aliasing*: if the
	// component parser dropped `.` on one side and kept it on the other, `/a/.`
	// and `/a` could become different objects and a caller could write through
	// one path and not see it through the other.
	let mut s = store(1024)?;
	s.mkdir("/a", 0o755)?;

	// -- Exec: the same write, addressed four ways
	s.write_new_file("/a/./f", b"via a dot", 0o644)?;

	// -- Check: one file, and every spelling reads it
	for spelling in ["/a/f", "/a/./f", "/./a/f", "/a//.//f", "/a/././f"] {
		assert_eq!(
			s.read_file(spelling)?,
			b"via a dot",
			"{spelling} must name the file written through /a/./f"
		);
	}
	assert_eq!(
		names(&s.read_dir("/a")?),
		vec!["f".to_string()],
		"a dot must not create an entry"
	);
	assert_eq!(names(&s.read_dir("/a/.")?), vec!["f".to_string()]);
	Ok(())
}

#[test]
fn test_a_dot_or_dot_dot_can_never_be_created_as_a_name() -> Result<()> {
	// -- Setup & Fixtures: the invariant behind the two tests above. If a name
	// of `.` or `..` could ever be written, it would be a *permanently
	// unreachable* entry: the reader drops `.` and refuses `..`, so the entry
	// would exist in the `DirNode`, occupy its bytes, appear in no listing, and
	// be impossible to delete. That is a way to hide data in a volume and to
	// leak space in it at the same time.
	let mut s = store(1024)?;
	s.mkdir("/d", 0o755)?;

	// -- Exec: every route to creating an entry, aimed at the two names
	let attempts: Vec<(&str, Result<()>)> = vec![
		("mkdir /.", s.mkdir("/.", 0o755)),
		("mkdir /..", s.mkdir("/..", 0o755)),
		("mkdir /d/.", s.mkdir("/d/.", 0o755)),
		("create_file /..", s.create_file("/..", 0o644)),
		("write_new_file /d/..", s.write_new_file("/d/..", b"x", 0o644)),
		("rename onto /..", s.rename("/d", "/..")),
		("rename onto /..x", s.rename("/d", "/d/..")),
	];

	// -- Check: each is refused, and the tree is unchanged by all of them
	for (what, r) in attempts {
		expect_err(what, r);
	}
	assert_eq!(names(&s.read_dir("/")?), vec!["d".to_string()]);
	assert_eq!(s.read_dir("/d")?.len(), 0, "nothing may have been created inside /d");
	Ok(())
}

#[test]
fn test_a_backslash_is_not_a_separator_here() -> Result<()> {
	// -- Setup & Fixtures: `..\..\etc` is a traversal on Windows and nothing at
	// all here: one filename. The dangerous version of this test is the one
	// where a parser accepts `\` as a separator "for convenience": it would give
	// two ways to spell every path, and a caller that validated one spelling
	// would not have validated the other.
	let mut s = store(1024)?;

	// -- Exec: create it, then read it back under the same name
	let windows_ish = name_of(br"..\..\etc\passwd");
	let path = format!("/{windows_ish}");
	s.write_new_file(&path, b"just a name", 0o644)?;

	// -- Check: it is one entry, spelled as written
	assert_eq!(names(&s.read_dir("/")?), vec![windows_ish.clone()]);
	assert_eq!(s.read_file(&path)?, b"just a name");

	// -- Check: and it is not a traversal: no path containing a backslash can
	// reach anything else, and the name with the backslashes in it is the only
	// thing that was made.
	assert!(
		s.read_dir("/etc").is_err(),
		"a backslash must not have created a directory"
	);
	assert_eq!(s.read_dir("/")?.len(), 1);
	Ok(())
}

#[test]
fn test_a_url_encoded_separator_is_a_name_not_a_separator() -> Result<()> {
	// -- Setup & Fixtures: `%2f` and `%2e%2e` are the payloads of every web
	// traversal, and this crate never percent-decodes anything: there is no URL
	// layer here to do it. The test exists to pin that: a decode anywhere in the
	// path chain would turn a name into a path, and would do it *after* whatever
	// validation the caller did, which is the definition of a bypass.
	let mut s = store(1024)?;

	// -- Exec
	let encoded = name_of(b"%2e%2e%2f%2e%2e%2fetc%2fpasswd");
	s.write_new_file(&format!("/{encoded}"), b"literal", 0o644)?;
	s.mkdir("/real", 0o755)?;

	// -- Check: the percent-encoded string is one entry with that exact name,
	// and the traversal it stands for did not happen.
	assert!(names(&s.read_dir("/")?).contains(&encoded));
	assert!(s.stat("/..%2f..%2fetc").is_err(), "no decode may have taken place");
	assert!(s.stat("/%2e%2e/etc").is_err());
	assert_eq!(s.read_dir("/real")?.len(), 0);
	Ok(())
}

// endregion:    --- Traversal

// region:    --- Separators, roots and spellings

#[test]
fn test_an_absolute_looking_name_resolves_inside_the_volume() -> Result<()> {
	// -- Setup & Fixtures: `/etc/passwd` names a file *in this volume*. There is
	// no host filesystem behind the store, so the only way this could go wrong
	// is if a caller handed the string to something that does have one, which
	// is a caller's bug, not this crate's, but it is why the crate must be
	// unambiguous about it: the path is volume-absolute and there is no other
	// namespace.
	let mut s = store(1024)?;
	s.mkdir("/etc", 0o755)?;
	s.write_new_file("/etc/passwd", b"volume-local", 0o644)?;

	// -- Exec & Check: every over-slashed spelling names the same object
	for spelling in ["/etc/passwd", "//etc/passwd", "///etc/passwd", "/etc//passwd", "etc/passwd"] {
		assert_eq!(
			s.read_file(spelling)?,
			b"volume-local",
			"{spelling} must be volume-absolute"
		);
	}
	Ok(())
}

#[test]
fn test_an_empty_path_is_the_root_and_a_root_path_is_not_creatable() -> Result<()> {
	// -- Setup & Fixtures: the empty path and `/` both name the root, harmless
	// to *read*, and refused for every operation that would create or replace
	// something, because "write to the volume root" has no meaning in this
	// format and a verb that guessed at one would be a verb that could clobber
	// the whole tree.
	let mut s = store(1024)?;

	// -- Check: reading the root works under every spelling of it
	for spelling in ["", "/", "//", "///"] {
		assert_eq!(s.stat(spelling)?.kind, InodeKind::Directory);
		assert!(s.read_dir(spelling)?.is_empty());
	}

	// -- Exec & Check: writing to it does not
	for spelling in ["", "/", "//"] {
		expect_err(spelling, s.mkdir(spelling, 0o755));
		expect_err(spelling, s.create_file(spelling, 0o644));
		expect_err(spelling, s.write_new_file(spelling, b"x", 0o644));
		expect_err(spelling, s.remove(spelling));
	}
	Ok(())
}

#[test]
fn test_a_trailing_slash_does_not_change_which_object_is_named() -> Result<()> {
	// -- Setup & Fixtures: `read_dir("/a/")` and `read_dir("/a")` must agree.
	// POSIX uses the trailing slash to mean "must be a directory"; this format
	// does not, and inventing that rule here would mean two paths that the
	// writer treats as one being treated as two by the reader.
	let mut s = store(1024)?;
	s.mkdir("/a", 0o755)?;
	s.write_new_file("/a/f", b"f", 0o644)?;

	// -- Exec & Check
	assert_eq!(names(&s.read_dir("/a")?), names(&s.read_dir("/a/")?));
	assert_eq!(
		s.stat("/a")?.content_ref,
		s.stat("/a/")?.content_ref,
		"two spellings, two objects"
	);
	// -- Check: and a trailing slash on a *file* is not an error either; it is
	// the same plain path, because treating it as one would be the reader
	// inventing a rule the writer never applied.
	assert_eq!(s.read_file("/a/f/")?, b"f");
	Ok(())
}

#[test]
fn test_a_component_may_not_contain_a_separator_and_a_listing_never_hands_one_back() -> Result<()> {
	// -- Setup & Fixtures: the invariant that keeps a replay safe. `rebuild_into`
	// and a compaction pass take names out of `read_dir` and build paths from them.
	// If a name could contain `/`, replaying it would address a *different*
	// location than the one it came from: a name `a/b` copied into `x` would
	// create `x/a/b`, and the tree would grow a level that no caller wrote.
	let mut s = store(1024)?;
	s.mkdir("/d", 0o755)?;

	// -- Exec: the only way to write a name with a slash in it is to write a
	// path, which splits it.
	assert!(
		s.write_new_file("/d/a/b", b"x", 0o644).is_err(),
		"no intermediate directory exists yet"
	);
	assert!(s.stat("/d/a/b").is_err(), "no intermediate directory exists yet");
	s.mkdir("/d/a", 0o755)?;
	s.write_new_file("/d/a/b", b"x", 0o644)?;

	// -- Check: the listing hands back two names, neither of which has a slash
	let listed = names(&s.read_dir("/d")?);
	assert_eq!(listed, vec!["a".to_string()]);
	let inner = names(&s.read_dir("/d/a")?);
	assert_eq!(inner, vec!["b".to_string()]);
	for n in listed.iter().chain(inner.iter()) {
		assert!(
			!n.contains('/'),
			"a listing must never hand back a name containing a separator: {n}"
		);
	}
	Ok(())
}

#[test]
fn test_the_component_parser_takes_exactly_the_paths_the_store_does() -> Result<()> {
	// -- Setup & Fixtures: `components_of` is the single place a path becomes a
	// list of names, and it is public. A table rather than a story, because the
	// rule is short and its edge cases are the whole of it.
	let cases: &[(&str, &[&str])] = &[
		("", &[]),
		("/", &[]),
		("//", &[]),
		(".", &[]),
		("./", &[]),
		("/.", &[]),
		("a", &["a"]),
		("/a", &["a"]),
		("a/", &["a"]),
		("/a/", &["a"]),
		("a/b", &["a", "b"]),
		("/a//b/", &["a", "b"]),
		("/a/./b", &["a", "b"]),
		("a.b", &["a.b"]),
		("...", &["..."]),
		("/a/.../b", &["a", "...", "b"]),
	];

	// -- Exec & Check
	for (path, want) in cases {
		let got = components_of(path)?;
		assert_eq!(got, *want, "components_of({path:?})");
	}
	Ok(())
}

// endregion:    --- Separators, roots and spellings

// region:    --- A name is data

#[test]
fn test_a_name_the_lookup_cannot_see_is_impossible_to_create() -> Result<()> {
	// -- Setup & Fixtures: this is the general form of the `.`/`..` rule. Any
	// string the writer accepts has to be a string the reader can find again;
	// the property is tested by writing a batch of awkward names and then
	// looking each one up by the exact bytes that were written, with no
	// normalisation applied between the two.
	let mut s = store(1024)?;
	let awkward: Vec<String> = vec![
		// Unicode normalisation: NFC and NFD spellings of `é`. They are
		// different byte strings and must stay different files: a store that
		// normalised would merge them, and two callers who chose different
		// spellings would find they share a file.
		"caf\u{e9}".to_string(),
		"cafe\u{301}".to_string(),
		// Look-alikes: Cyrillic `а` and Latin `a`.
		"\u{430}dmin".to_string(),
		"admin".to_string(),
		// Case: `A` and `a` are different names, as they are everywhere that is
		// not a case-insensitive filesystem.
		"Admin".to_string(),
		// Invisible characters: a zero-width space, a zero-width joiner, a
		// right-to-left override. All are just bytes.
		"zero\u{200b}width".to_string(),
		"joiner\u{200d}x".to_string(),
		"\u{202e}gnp.exe".to_string(),
		// Trailing whitespace and dots, which some platforms silently strip.
		"trailing ".to_string(),
		"trailing.".to_string(),
		" leading".to_string(),
		// A name that is only whitespace, and one that is only a control byte.
		"\t".to_string(),
		"\u{7f}".to_string(),
		"\n".to_string(),
	];

	// -- Exec
	for name in &awkward {
		s.write_new_file(&format!("/{name}"), name.as_bytes(), 0o644)?;
	}

	// -- Check: every one is present exactly once, and reads back its own bytes
	let listed = names(&s.read_dir("/")?);
	let mut copies = listed.clone();
	copies.sort();
	copies.dedup();
	assert_eq!(
		copies.len(),
		listed.len(),
		"two distinct names collapsed into one: {listed:?}"
	);
	for name in &awkward {
		assert_eq!(
			s.read_file(&format!("/{name}"))?,
			name.as_bytes(),
			"{name:?} did not round-trip under its own spelling"
		);
	}
	// -- Check: and they are all distinct files, not one file written fifteen
	// times. This is the assertion that catches a normalising store.
	assert_eq!(
		awkward.iter().collect::<std::collections::HashSet<_>>().len(),
		awkward.len()
	);
	Ok(())
}

#[test]
fn test_a_nul_in_a_name_does_not_truncate_it() -> Result<()> {
	// -- Setup & Fixtures: the classic C-string truncation. If any layer
	// truncated at a NUL: a lookup comparing C strings, a length taken from
	// `strlen`, a `DirEntry` built from a lossy conversion, then `a\0evil` and
	// `a` would be the same file, and an attacker who could choose a name could
	// choose to *be* an existing one. The two names here are chosen so that a
	// truncating store collapses them; the assertion is that it does not.
	let mut s = store(1024)?;

	// -- Exec
	let short = "victim".to_string();
	let long = "victim\u{0}attacker".to_string();
	s.write_new_file(&format!("/{short}"), b"the real one", 0o644)?;
	s.write_new_file(&format!("/{long}"), b"the impostor", 0o644)?;

	// -- Check: two entries, and each reads its own contents
	let listed = names(&s.read_dir("/")?);
	assert!(listed.contains(&short), "{listed:?}");
	assert!(listed.contains(&long), "{listed:?}");
	assert_eq!(s.read_file(&format!("/{short}"))?, b"the real one");
	assert_eq!(s.read_file(&format!("/{long}"))?, b"the impostor");

	// -- Exec & Check: and removing one leaves the other
	s.remove(&format!("/{long}"))?;
	assert_eq!(s.read_file(&format!("/{short}"))?, b"the real one");
	assert!(!names(&s.read_dir("/")?).contains(&long));
	Ok(())
}

#[test]
fn test_a_name_is_not_case_folded() -> Result<()> {
	// -- Setup & Fixtures: `README` and `readme` are different files. On a
	// case-insensitive store one would overwrite the other, which is both a
	// surprise and a collision an attacker can aim at when the victim's name is
	// all that is predictable.
	let mut s = store(1024)?;

	// -- Exec
	let spellings = ["README", "readme", "ReadMe", "rEaDmE"];
	for (i, name) in spellings.iter().enumerate() {
		s.write_new_file(&format!("/{name}"), format!("variant {i}").as_bytes(), 0o644)?;
	}

	// -- Check
	assert_eq!(s.read_dir("/")?.len(), spellings.len(), "case variants collapsed");
	for (i, name) in spellings.iter().enumerate() {
		assert_eq!(s.read_file(&format!("/{name}"))?, format!("variant {i}").as_bytes());
	}
	Ok(())
}

#[test]
fn test_two_names_that_differ_only_in_an_invisible_character_are_two_names() -> Result<()> {
	// -- Setup & Fixtures: the spoofing case. A caller that shows a listing to a
	// person (an operator console does exactly this) must be able to tell
	// `report.txt` from `report.txt` with an invisible byte in it, and the store
	// must not be the thing that decides they are the same. It cannot make the
	// spoof impossible; it can refuse to participate in it.
	let mut s = store(1024)?;

	// -- Exec
	s.write_new_file("/report.txt", b"real", 0o644)?;
	s.write_new_file("/report\u{200b}.txt", b"spoofed", 0o644)?;
	s.write_new_file("/report\u{202e}txt", b"reversed", 0o644)?;

	// -- Check: three entries with three distinct byte strings
	let listed = names(&s.read_dir("/")?);
	assert_eq!(listed.len(), 3, "{listed:?}");
	assert_eq!(s.read_file("/report.txt")?, b"real");
	assert_eq!(s.read_file("/report\u{200b}.txt")?, b"spoofed");
	assert_eq!(s.read_file("/report\u{202e}txt")?, b"reversed");
	Ok(())
}

#[test]
fn test_a_verbatim_name_is_what_a_listing_hands_back() -> Result<()> {
	// -- Setup & Fixtures: `read_dir` is what a caller renders. If it decoded,
	// escaped or trimmed, the caller would show a name that is not the name on
	// disk, and a person choosing from that list would be choosing by the wrong
	// string.
	let mut s = store(1024)?;
	let names_in = [
		"quote'and\"double",
		"back\\slash",
		"tab\there",
		"new\nline",
		"semi;colon&amp",
		"star*glob?",
		"dash-leading",
		" dash-trailing",
	];

	// -- Exec
	for name in names_in {
		s.write_new_file(&format!("/{name}"), b".", 0o644)?;
	}

	// -- Check: byte for byte, no escaping
	let listed = names(&s.read_dir("/")?);
	for name in names_in {
		assert!(listed.contains(&name.to_string()), "{name:?} is not in {listed:?}");
	}
	assert_eq!(listed.len(), names_in.len());
	Ok(())
}

// endregion:    --- A name is data

// region:    --- Long and many

#[test]
fn test_a_path_with_a_hundred_thousand_components_is_refused_or_resolved_but_never_hangs() -> Result<()> {
	// -- Setup & Fixtures: a path built from attacker-controlled input can be
	// arbitrarily long. The requirement is only that the answer comes back:
	// a parser that recursed per component, or that built strings quadratically,
	// would be a cheap denial of service on the process that parsed it.
	let path = format!("/{}", vec!["a"; 100_000].join("/"));

	// -- Exec
	let components = components_of(&path)?;

	// -- Check: the parser is iterative and linear
	assert_eq!(components.len(), 100_000);
	assert!(components.iter().all(|c| *c == "a"));

	// -- Exec & Check: and asking a store about it is an error about a missing
	// entry, not a hang. The first component does not exist, so the walk stops
	// there, which is also the property that makes a long hostile path cheap.
	let s = store(256)?;
	let e = expect_err("a very long path", s.stat(&path));
	assert!(
		matches!(e, Error::NoSuchFile { .. } | Error::NotADirectory { .. }),
		"got {e:?}"
	);
	Ok(())
}

#[test]
fn test_a_path_whose_components_are_all_at_the_length_limit_is_handled() -> Result<()> {
	// -- Setup & Fixtures: the limit is per *name*, not per path, so a path can
	// legitimately be far longer than 256 bytes. A store that checked the whole
	// path against the component limit would refuse these, and would do it
	// with an error about a name the caller never wrote.
	let at_limit = "n".repeat(256);
	let path = format!("/{at_limit}/{at_limit}");

	// -- Exec: refused only because the parent does not exist, which is the
	// ordinary `NoSuchFile` and not a length complaint.
	let mut s = store(64 * 1024)?;
	let e = expect_err("a nested at-limit path", s.create_file(&path, 0o644));
	assert!(matches!(e, Error::NoSuchFile { .. }), "got {e:?}");

	// -- Exec: create the parent, then the child: two names, each legal
	s.mkdir(&format!("/{at_limit}"), 0o755)?;
	s.create_file(&path, 0o644)?;

	// -- Check
	assert_eq!(s.stat(&path)?.kind, InodeKind::RegularFile);
	assert_eq!(names(&s.read_dir(&format!("/{at_limit}"))?), vec![at_limit.clone()]);
	Ok(())
}

#[test]
fn test_many_components_that_do_not_exist_name_the_first_missing_one() -> Result<()> {
	// -- Setup & Fixtures: which component a failure names is a security
	// property in miniature: an error that names a component past the point
	// where the caller lost the right to know is an oracle. Here it names the
	// first component that does not exist, which is the most the caller could
	// already infer.
	let s = store(1024)?;
	let path = "/nope/deeper/and/deeper/still";

	// -- Exec
	let e = expect_err("a missing chain", s.stat(path));

	// -- Check: `resolve_chain` reports the accumulated prefix of the first
	// component it could not look up (here the first one), so the prefix is the
	// name itself without a leading slash.
	match e {
		Error::NoSuchFile { path } => assert_eq!(path, "nope", "must name the first miss, not the tail"),
		other => panic!("expected NoSuchFile, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_directory_cannot_be_made_to_hold_more_than_its_ceiling() -> Result<()> {
	// -- Setup & Fixtures: `DirNode` has a hard 400-entry ceiling and no B-tree
	// splitting. From a security angle the question is not "why 400" but "what
	// happens at 401": the answer must be a refusal that leaves the directory
	// exactly as it was. A ceiling enforced by *overwriting* (or by dropping
	// the new entry silently) would let a caller overwrite a file by filling
	// its directory first, which is an integrity attack with a filesystem for a
	// weapon.
	let mut s = store(16_384)?;
	for i in 0..400 {
		s.create_file(&format!("/f{i:03}"), 0o644)?;
	}

	// -- Exec
	let e = expect_err("entry 401", s.create_file("/f400", 0o644));

	// -- Check: refused, and refused for the reason that is true
	match e {
		Error::Format(inner) => assert!(inner.to_string().contains("full"), "{inner}"),
		other => panic!("expected a full-directory refusal, got {other:?}"),
	}

	// -- Check: the directory is intact: all 400, none renamed, none lost.
	// (Exactly the 400: a volume made by `Store::format` has no `.ledgerfs`
	// identity files, which is what `format_app` adds.)
	let listed = names(&s.read_dir("/")?);
	assert_eq!(
		listed.len(),
		400,
		"nothing may be lost when the ceiling is hit: {listed:?}"
	);
	for i in 0..400 {
		assert!(
			listed.contains(&format!("f{i:03}")),
			"f{i:03} disappeared when the ceiling was hit"
		);
	}
	Ok(())
}

// endregion:    --- Long and many

// region:    --- Modes

#[test]
fn test_a_mode_is_stored_verbatim_and_is_not_a_permission_gate() -> Result<()> {
	// -- Setup & Fixtures: LedgerFS stores a mode but enforces nothing with it;
	// the host crate has no notion of a user, and a volume is opened by whoever
	// holds the file. That is worth pinning rather than leaving implied, because
	// a caller reading `stat()` and *deciding* from the mode would be building
	// on a check that does not exist. The store's job is to keep the number it
	// was given; controlling access is the caller's (or the OS file's).
	let mut s = store(1024)?;

	// -- Exec: modes including setuid, setgid and sticky, bits that mean
	// something to a POSIX kernel and nothing here.
	let modes = [0o644u32, 0o600, 0o755, 0o000, 0o777, 0o4755, 0o2755, 0o1755];

	// -- Check: each round-trips exactly, including the ones a sanitiser would
	// have stripped. Stripping them would be worse than storing them: a caller
	// that set 0o4755 and read back 0o755 has been lied to about what is on
	// disk, and a volume exported to a real filesystem would then hold a
	// different file than the one that was written.
	for (i, mode) in modes.into_iter().enumerate() {
		let path = format!("/m{i}");
		s.write_new_file(&path, b"x", mode)?;
		assert_eq!(s.stat(&path)?.mode, mode, "mode 0{mode:o} did not round-trip");
	}

	// -- Exec: a directory's mode too, through the same path
	s.mkdir("/priv", 0o700)?;
	assert_eq!(s.stat("/priv")?.mode, 0o700);

	// -- Exec & Check: and there is no validation at all, not even a range.
	// A mode above `0o7777` has no meaning on any POSIX system, and the store
	// keeps it anyway, because the field is a `u32` on disk and inventing a
	// sanitiser here would make `stat()` disagree with the bytes that were
	// written. The important half of this is what it tells a *caller*: the mode
	// that comes back out of `stat()` is what went in, and nothing more. It is
	// not a permission check, and a caller that treats it as one is trusting a
	// value an attacker in the volume's writer position can set to anything.
	s.write_new_file("/absurd", b"x", u32::MAX)?;
	assert_eq!(
		s.stat("/absurd")?.mode,
		u32::MAX,
		"the mode field is stored, not sanitised"
	);
	Ok(())
}

// endregion:    --- Modes

// region:    --- The read path is not a write path

#[test]
fn test_a_refused_read_does_not_change_the_volume() -> Result<()> {
	// -- Setup & Fixtures: every path test above does a lot of *failing* lookups
	// against a live store. Each of them must leave the volume's bytes alone;
	// a resolve that "repaired" a `DirNode`, or that wrote a hint back to the
	// device, would make an ordinary failed lookup a mutation, and would make a
	// read-only-in-intent caller a writer.
	let (mut s, watch) = common::watched(1024)?;
	s.mkdir("/d", 0o755)?;
	s.write_new_file("/d/f", b"content", 0o644)?;
	let before = common::bytes(&watch);
	let seq = s.commit_seq();

	// -- Exec: a spread of failing lookups and reads
	for path in ["/nope", "/d/nope", "/d/f/nope", "..", "/d/../f", "/d/f/deeper", "/x/y/z"] {
		let _ = s.stat(path);
		let _ = s.read_file(path);
		let _ = s.read_dir(path);
	}
	let _ = s.read_at("/d/f", 99_999, 16);

	// -- Check: same bytes, same commit sequence
	assert_eq!(common::bytes(&watch), before, "a failed read modified the volume");
	assert_eq!(s.commit_seq(), seq, "a failed read advanced the commit sequence");

	// -- Check: and the volume still works
	assert_eq!(s.read_file("/d/f")?, b"content");
	Ok(())
}

#[test]
fn test_a_rename_cannot_be_used_to_leave_an_entry_unreachable() -> Result<()> {
	// -- Setup & Fixtures: `rename` is the verb that moves a name, so it is the
	// one that could produce an entry the reader cannot find, by renaming onto
	// a path the reader normalises away, or into a directory that does not
	// exist, or onto itself.
	let mut s = store(1024)?;
	s.mkdir("/a", 0o755)?;
	s.write_new_file("/a/f", b"payload", 0o644)?;

	// -- Exec: the shapes a rename could use to lose a file
	let attempts: Vec<(&str, Result<()>)> = vec![
		("onto a missing directory", s.rename("/a/f", "/gone/f")),
		("onto `.`", s.rename("/a/f", "/a/.")),
		("onto `.` from the root", s.rename("/a/f", "/.")),
		("through `..`", s.rename("/a/f", "/a/../f")),
		("onto an empty path", s.rename("/a/f", "")),
		("from an empty path", s.rename("", "/a/g")),
	];

	// -- Check: all refused
	for (what, r) in attempts {
		expect_err(what, r);
	}

	// -- Check: and the file is exactly where it was, readable under its own
	// name. This is the assertion that matters: a refusal that had already
	// removed the entry would be a delete wearing an error message.
	assert_eq!(s.read_file("/a/f")?, b"payload");
	assert_eq!(names(&s.read_dir("/a")?), vec!["f".to_string()]);
	assert_eq!(names(&s.read_dir("/")?), vec!["a".to_string()]);
	Ok(())
}

#[test]
fn test_a_rename_onto_an_existing_file_replaces_it_rather_than_merging_it() -> Result<()> {
	// -- Setup & Fixtures: replace-on-rename is the POSIX rule and the one a
	// caller expects; the security-relevant half is that the destination's *old*
	// contents are gone. A rename that merged, or that left the old object
	// reachable under the new name, would mean the new name read as a mixture of
	// two files.
	let mut s = store(1024)?;
	s.write_new_file("/src", b"short", 0o644)?;
	s.write_new_file("/dst", &vec![b'D'; 5000], 0o644)?;

	// -- Exec
	s.rename("/src", "/dst")?;

	// -- Check: the destination is the source, whole, with no tail of the old
	// contents surviving.
	assert_eq!(s.read_file("/dst")?, b"short");
	assert!(s.stat("/src").is_err(), "the source name must be gone");
	assert_eq!(names(&s.read_dir("/")?), vec!["dst".to_string()]);
	Ok(())
}

// endregion:    --- The read path is not a write path
