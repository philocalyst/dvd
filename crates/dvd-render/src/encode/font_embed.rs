//! Subsetting and embedding font faces into an SVG document.
//!
//! `subsetter` builds compact fonts for PDF's CID-keyed glyph addressing:
//! outline data survives, but `cmap` (and `GSUB`/`GPOS`) do not — a PDF
//! content stream addresses glyphs directly by ID and never needs any of
//! them (see the crate's own doc comment). An `@font-face`'d SVG `<text>`
//! element is the opposite: the browser is handed real Unicode text and has
//! to look glyphs up itself, which needs a working `cmap`.
//!
//! This module bridges the two. [`build_font_face`] runs `subsetter` for the
//! compact glyf/CFF + head/hmtx/hhea/maxp/post/name tables it produces, then
//! splices in a synthesised `cmap` (format 12) built from exactly the
//! codepoint→glyph associations this recording used, plus the original
//! face's `OS/2` table carried over verbatim.
//!
//! `OS/2` is not optional in practice: `subsetter` drops it for the same
//! PDF-CID reason it drops `cmap` (a PDF viewer doesn't need it either), but
//! a browser's web font loader runs everything through OTS (the OpenType
//! Sanitizer), which rejects a TrueType-flavoured web font that lacks it —
//! Chromium reports exactly that as `FontFace.load()` rejecting with a
//! generic "Invalid font data" `SyntaxError`, with no table name attached,
//! which is what makes it easy to misdiagnose as a `cmap`/checksum bug
//! instead. `OS/2` carries only font-wide metrics (weight class, average
//! width, Unicode/codepage coverage bits and so on) — nothing in it
//! references a glyph ID — so the original table's bytes are valid for the
//! subsetted font completely unmodified.
//!
//! What is deliberately *not* reconstructed is `GSUB`/`GPOS`. `svg.rs` only
//! ever calls this for a character once it has confirmed — via
//! `Shaper::cmap_glyph` — that the font's own plain cmap lookup for that
//! character lands on the exact glyph shaping already chose. That is true
//! for the overwhelming majority of terminal content (no contextual
//! substitution happens for ASCII, CJK, box drawing or Nerd Font icons), so
//! a cmap-only subset reproduces it exactly. The characters that fail that
//! check (ligatures, contextual forms) never reach this module at all — they
//! stay on the pre-existing outline/`<path>` route. RTL runs are the one
//! deliberate exception on the caller's side: they skip this module
//! entirely and render as plain `<text dir="rtl">` with no forced
//! `font-family`, trusting the browser's own bidi engine and the host's
//! system Arabic fallback (which has real `GSUB`) rather than reproducing
//! joining ourselves.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use subsetter::GlyphRemapper;

/// Subset `data` (one font face's raw bytes; `index` selects the face within
/// a collection) down to exactly the glyphs reachable through `codepoints`,
/// and return a complete `@font-face { ... }` CSS rule, ready to drop into a
/// `<style>` element, naming the face `css_family`.
///
/// `codepoints` maps each character this recording actually drew with this
/// face to the glyph ID shaping chose for it — see the module doc for why
/// that is always the same glyph a plain cmap lookup would give by the time
/// a character reaches here.
pub fn build_font_face(
	css_family: &str,
	data: &[u8],
	index: u32,
	codepoints: &BTreeMap<char, u32>,
) -> Result<String> {
	anyhow::ensure!(!codepoints.is_empty(), "no codepoints to embed");

	let mut remapper = GlyphRemapper::new();
	let mut cmap_entries: Vec<(u32, u16)> = Vec::with_capacity(codepoints.len());

	for (&character, &glyph_id) in codepoints {
		// `subsetter`'s remapper is 16-bit, matching every real-world font's
		// glyph count. A face that somehow exceeds that is skipped rather
		// than failing the whole face's embedding.
		let Ok(old_gid) = u16::try_from(glyph_id) else {
			continue;
		};
		let new_gid = remapper.remap(old_gid);
		cmap_entries.push((character as u32, new_gid));
	}

	anyhow::ensure!(!cmap_entries.is_empty(), "no 16-bit glyph ids to embed");

	// Read out of the *original* face, before `subsetter` ever gets to drop
	// it — see the module doc for why a browser's web font loader needs
	// this present even though `subsetter` (built for PDF) does not.
	let os2 = find_table(data, index, b"OS/2").context("reading the original OS/2 table")?;

	let subset = subsetter::subset(data, index, &remapper).context("subsetting font face")?;
	let font = splice_extra_tables(&subset, &cmap_entries, os2.as_deref())
		.context("splicing the synthesised cmap and carried-over OS/2 table")?;

	let (format, extension) = sfnt_format(&font);
	let encoded = BASE64.encode(&font);

	Ok(format!(
		"@font-face{{font-family:'{css_family}';src:url(data:font/{extension};base64,{encoded}) format('{format}');}}"
	))
}

/// Whether the sfnt at the start of `font` is CFF- or glyf-flavoured, as the
/// `(format() keyword, data: URI file extension)` pair each implies.
fn sfnt_format(font: &[u8]) -> (&'static str, &'static str) {
	if font.get(0..4) == Some(b"OTTO") {
		("opentype", "otf")
	} else {
		("truetype", "ttf")
	}
}

/// Parse `font` (a `subsetter` output) as an sfnt, add a synthesised `cmap`
/// table built from `entries` plus `os2` (the original face's `OS/2` table,
/// carried over verbatim — see the module doc for why), and rebuild the
/// whole file — table directory, per-table checksums and the `head` table's
/// `checkSumAdjustment` included — exactly as `subsetter`'s own
/// `construct()` does, since it emits neither table itself.
fn splice_extra_tables(font: &[u8], entries: &[(u32, u16)], os2: Option<&[u8]>) -> Result<Vec<u8>> {
	let sfnt_version = read_u32(font, 0)?;
	let mut tables = read_sfnt_tables(font)?;
	tables.retain(|(tag, _)| tag != b"cmap" && tag != b"OS/2");
	tables.push((*b"cmap", build_cmap_table(entries)));
	if let Some(os2) = os2 {
		tables.push((*b"OS/2", os2.to_vec()));
	}
	Ok(rebuild_sfnt(sfnt_version, tables))
}

/// A single contiguous stretch of the synthesised cmap: consecutive
/// codepoints mapped to consecutive glyph ids, merged so a long run of
/// ordinary text costs one `cmap` group instead of one per character.
struct CodepointGroup {
	start_char: u32,
	end_char: u32,
	start_glyph: u32,
	end_glyph: u32,
}

/// Build a minimal format-12 `cmap` subtable — segmented coverage, the only
/// format that can address the full Unicode range a recording might use
/// (CJK and beyond), under a single (platform 3, encoding 10) entry that
/// every mainstream engine (browsers, `ttf-parser`/`resvg`) resolves.
fn build_cmap_table(entries: &[(u32, u16)]) -> Vec<u8> {
	let mut sorted = entries.to_vec();
	sorted.sort_by_key(|&(character, _)| character);
	sorted.dedup_by_key(|&mut (character, _)| character);

	let mut groups: Vec<CodepointGroup> = Vec::new();
	for (character, glyph_id) in sorted {
		if let Some(last) = groups.last_mut() {
			if character == last.end_char + 1 && glyph_id as u32 == last.end_glyph + 1 {
				last.end_char = character;
				last.end_glyph = glyph_id as u32;
				continue;
			}
		}
		groups.push(CodepointGroup {
			start_char: character,
			end_char: character,
			start_glyph: glyph_id as u32,
			end_glyph: glyph_id as u32,
		});
	}

	let mut subtable = Vec::new();
	subtable.extend_from_slice(&12u16.to_be_bytes()); // format
	subtable.extend_from_slice(&0u16.to_be_bytes()); // reserved
	let length_field = subtable.len();
	subtable.extend_from_slice(&0u32.to_be_bytes()); // length, patched below
	subtable.extend_from_slice(&0u32.to_be_bytes()); // language
	subtable.extend_from_slice(&(groups.len() as u32).to_be_bytes()); // numGroups
	for group in &groups {
		subtable.extend_from_slice(&group.start_char.to_be_bytes());
		subtable.extend_from_slice(&group.end_char.to_be_bytes());
		subtable.extend_from_slice(&group.start_glyph.to_be_bytes());
	}
	let length = subtable.len() as u32;
	subtable[length_field..length_field + 4].copy_from_slice(&length.to_be_bytes());

	let mut table = Vec::new();
	table.extend_from_slice(&0u16.to_be_bytes()); // version
	table.extend_from_slice(&1u16.to_be_bytes()); // numTables
	table.extend_from_slice(&3u16.to_be_bytes()); // platformID: Windows
	table.extend_from_slice(&10u16.to_be_bytes()); // encodingID: UCS-4
	table.extend_from_slice(&12u32.to_be_bytes()); // offset to subtable
	table.extend_from_slice(&subtable);
	table
}

/// Read a single font's table directory into `(tag, data)` pairs. `font` is
/// a plain (non-collection) sfnt, which is always true of a `subsetter`
/// output — `subsetter::subset` never produces a `.ttc`.
fn read_sfnt_tables(font: &[u8]) -> Result<Vec<([u8; 4], Vec<u8>)>> {
	read_sfnt_tables_at(font, 0)
}

/// Read the table directory starting at `directory_offset` bytes into
/// `font` — `0` for a plain sfnt, or wherever a `.ttc`'s header points for
/// one member of a collection (see [`table_directory_offset`]). Table data
/// offsets inside each record are always absolute into the whole file
/// regardless, so only the directory's own location varies.
fn read_sfnt_tables_at(font: &[u8], directory_offset: usize) -> Result<Vec<([u8; 4], Vec<u8>)>> {
	let num_tables = read_u16(font, directory_offset + 4)? as usize;
	let mut tables = Vec::with_capacity(num_tables);

	for index in 0..num_tables {
		let record_offset = directory_offset + 12 + index * 16;
		let record = font
			.get(record_offset..record_offset + 16)
			.context("sfnt table directory is truncated")?;

		let tag: [u8; 4] = record[0..4].try_into().expect("4-byte slice");
		let offset = u32::from_be_bytes(record[8..12].try_into().expect("4-byte slice")) as usize;
		let length = u32::from_be_bytes(record[12..16].try_into().expect("4-byte slice")) as usize;
		let data = font
			.get(offset..offset + length)
			.context("sfnt table data is out of range")?
			.to_vec();

		tables.push((tag, data));
	}

	Ok(tables)
}

/// Where `index`'s table directory starts inside `font` — byte `0` for a
/// plain sfnt, or the offset a `.ttc` (font collection) header names for
/// that member. macOS ships plenty of system faces as `.ttc`s, so a font a
/// recording was shaped against (`face.index` in `Shaper::face`) is not
/// always index `0` of a single-font file.
fn table_directory_offset(font: &[u8], index: u32) -> Result<usize> {
	if font.get(0..4) != Some(b"ttcf") {
		return Ok(0);
	}

	let num_fonts = read_u32(font, 8)?;
	anyhow::ensure!(
		index < num_fonts,
		"font collection index {index} out of range ({num_fonts} fonts)"
	);
	read_u32(font, 12 + index as usize * 4).map(|offset| offset as usize)
}

/// Find one table by tag in `font`'s `index`'th face (see
/// [`table_directory_offset`]), or `None` if that face doesn't have it.
fn find_table(font: &[u8], index: u32, tag: &[u8; 4]) -> Result<Option<Vec<u8>>> {
	let directory_offset = table_directory_offset(font, index)?;
	let tables = read_sfnt_tables_at(font, directory_offset)?;
	Ok(tables
		.into_iter()
		.find(|(t, _)| t == tag)
		.map(|(_, data)| data))
}

/// Rebuild a complete sfnt from `(tag, data)` pairs — the same layout
/// `subsetter::construct()` produces: tables sorted by tag, a directory with
/// the required binary-search fields, 4-byte-aligned table data, per-table
/// checksums, and a recomputed `head.checkSumAdjustment` covering the whole
/// file.
fn rebuild_sfnt(sfnt_version: u32, mut tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
	tables.sort_by_key(|(tag, _)| *tag);

	// Zero the `head` table's checksum-adjustment field before anything is
	// checksummed — the field's own value must not be part of the sum it
	// is corrected against.
	for (tag, data) in &mut tables {
		if tag == b"head" && data.len() >= 12 {
			data[8..12].fill(0);
		}
	}

	let count = tables.len() as u16;
	let entry_selector = (count.max(1) as f32).log2().floor() as u16;
	let search_range = 2u16
		.saturating_pow(entry_selector as u32)
		.saturating_mul(16);
	let range_shift = count.saturating_mul(16).saturating_sub(search_range);

	let mut out = Vec::new();
	out.extend_from_slice(&sfnt_version.to_be_bytes());
	out.extend_from_slice(&count.to_be_bytes());
	out.extend_from_slice(&search_range.to_be_bytes());
	out.extend_from_slice(&entry_selector.to_be_bytes());
	out.extend_from_slice(&range_shift.to_be_bytes());

	let mut offset = 12 + tables.len() * 16;
	let mut head_checksum_field = None;
	let mut records = Vec::with_capacity(tables.len());

	for (tag, data) in &tables {
		if tag == b"head" {
			head_checksum_field = Some(offset + 8);
		}
		records.push((*tag, table_checksum(data), offset as u32, data.len() as u32));
		offset += data.len();
		offset = offset.div_ceil(4) * 4;
	}

	for (tag, checksum, table_offset, length) in &records {
		out.extend_from_slice(tag);
		out.extend_from_slice(&checksum.to_be_bytes());
		out.extend_from_slice(&table_offset.to_be_bytes());
		out.extend_from_slice(&length.to_be_bytes());
	}

	for (_, data) in &tables {
		out.extend_from_slice(data);
		while out.len() % 4 != 0 {
			out.push(0);
		}
	}

	if let Some(field_offset) = head_checksum_field {
		let file_checksum = table_checksum(&out);
		let adjustment = 0xB1B0AFBAu32.wrapping_sub(file_checksum);
		out[field_offset..field_offset + 4].copy_from_slice(&adjustment.to_be_bytes());
	}

	out
}

/// The OpenType table checksum: the sum of the table's bytes read as
/// big-endian `u32`s, the tail padded with zero if the length is not a
/// multiple of four.
fn table_checksum(data: &[u8]) -> u32 {
	let mut sum: u32 = 0;
	for chunk in data.chunks(4) {
		let mut word = [0u8; 4];
		word[..chunk.len()].copy_from_slice(chunk);
		sum = sum.wrapping_add(u32::from_be_bytes(word));
	}
	sum
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
	let bytes = data
		.get(offset..offset + 2)
		.context("sfnt header is truncated")?;
	Ok(u16::from_be_bytes(bytes.try_into().expect("2-byte slice")))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
	let bytes = data
		.get(offset..offset + 4)
		.context("sfnt header is truncated")?;
	Ok(u32::from_be_bytes(bytes.try_into().expect("4-byte slice")))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The synthesised `cmap` table must itself be a well-formed sfnt
	/// subtable a reader can walk: correct header, one encoding record, and
	/// a format-12 subtable whose declared length matches its actual size.
	#[test]
	fn cmap_table_round_trips_its_own_header_fields() {
		let entries = vec![('a' as u32, 1u16), ('b' as u32, 2u16), ('z' as u32, 9u16)];
		let table = build_cmap_table(&entries);

		assert_eq!(&table[0..2], &0u16.to_be_bytes(), "cmap version must be 0");
		assert_eq!(
			&table[2..4],
			&1u16.to_be_bytes(),
			"exactly one encoding record"
		);
		assert_eq!(&table[4..6], &3u16.to_be_bytes(), "platform 3 (Windows)");
		assert_eq!(&table[6..8], &10u16.to_be_bytes(), "encoding 10 (UCS-4)");

		let subtable_offset = 12usize;
		assert_eq!(
			&table[subtable_offset..subtable_offset + 2],
			&12u16.to_be_bytes()
		);
		let declared_length = u32::from_be_bytes(
			table[subtable_offset + 4..subtable_offset + 8]
				.try_into()
				.unwrap(),
		) as usize;
		assert_eq!(declared_length, table.len() - subtable_offset);
	}

	/// Two adjacent codepoints mapped to two adjacent glyph ids must merge
	/// into a single group — this is what keeps a run of ordinary text from
	/// costing one cmap group per character.
	#[test]
	fn contiguous_codepoints_and_glyphs_merge_into_one_group() {
		let entries = vec![
			('a' as u32, 10u16),
			('b' as u32, 11u16),
			('c' as u32, 12u16),
		];
		let table = build_cmap_table(&entries);
		let num_groups = u32::from_be_bytes(table[24..28].try_into().unwrap());
		assert_eq!(num_groups, 1, "a contiguous run must collapse to one group");
	}

	/// A gap in either the codepoints or the glyph ids must start a new
	/// group — merging across a gap would silently mis-map every character
	/// after it.
	#[test]
	fn a_gap_in_glyph_ids_starts_a_new_group() {
		let entries = vec![('a' as u32, 10u16), ('b' as u32, 99u16)];
		let table = build_cmap_table(&entries);
		let num_groups = u32::from_be_bytes(table[24..28].try_into().unwrap());
		assert_eq!(num_groups, 2);
	}

	/// `rebuild_sfnt` must reproduce a table directory a reader can walk
	/// back losslessly: the same tags and bytes come back out.
	#[test]
	fn rebuilt_sfnt_round_trips_its_tables() {
		let tables = vec![
			(*b"head", vec![0u8; 54]),
			(*b"cmap", vec![1, 2, 3]),
			(*b"glyf", vec![4, 5, 6, 7, 8]),
		];
		let font = rebuild_sfnt(0x0001_0000, tables);

		let parsed = read_sfnt_tables(&font).expect("rebuilt sfnt must parse");
		let mut tags: Vec<[u8; 4]> = parsed.iter().map(|(tag, _)| *tag).collect();
		tags.sort();
		assert_eq!(tags, vec![*b"cmap", *b"glyf", *b"head"]);

		let cmap = parsed.iter().find(|(tag, _)| tag == b"cmap").unwrap();
		assert_eq!(cmap.1, vec![1, 2, 3]);
	}

	/// `find_table` must locate a table in a plain (non-collection) sfnt —
	/// the shape every `subsetter` output and most real font files take.
	#[test]
	fn find_table_locates_a_table_in_a_plain_sfnt() {
		let tables = vec![
			(*b"head", vec![0u8; 54]),
			(*b"OS/2", vec![9, 9, 9]),
			(*b"glyf", vec![4, 5, 6, 7, 8]),
		];
		let font = rebuild_sfnt(0x0001_0000, tables);

		let found = find_table(&font, 0, b"OS/2").unwrap();
		assert_eq!(found, Some(vec![9, 9, 9]));

		let missing = find_table(&font, 0, b"GSUB").unwrap();
		assert_eq!(missing, None);
	}

	/// `splice_extra_tables` must carry `OS/2` over unmodified — a browser's
	/// web font loader (OTS) rejects a TrueType-flavoured web font that
	/// lacks it, which `subsetter` itself never emits (see the module doc).
	#[test]
	fn splice_extra_tables_carries_os2_over_verbatim() {
		let subsetter_output = rebuild_sfnt(
			0x0001_0000,
			vec![(*b"head", vec![0u8; 54]), (*b"glyf", vec![1, 2, 3])],
		);
		let entries = [('a' as u32, 1u16)];
		let os2 = [7u8; 96]; // a real OS/2 table is 96+ bytes; content is opaque here

		let spliced = splice_extra_tables(&subsetter_output, &entries, Some(&os2))
			.expect("splicing must succeed");

		let tables = read_sfnt_tables(&spliced).expect("spliced sfnt must parse");
		let os2_table = tables
			.iter()
			.find(|(tag, _)| tag == b"OS/2")
			.expect("OS/2 must be present in the spliced font");
		assert_eq!(
			os2_table.1, os2,
			"OS/2 bytes must be carried over unmodified"
		);

		let cmap_table = tables.iter().find(|(tag, _)| tag == b"cmap");
		assert!(
			cmap_table.is_some(),
			"cmap must still be spliced in alongside OS/2"
		);
	}
}
