package main

// MVDZ format version 1, written from FORMAT.md: container §2, META §3,
// INDEX §4, segments §5, validation §9.

import (
	"bytes"
	"crypto/sha256"
	"unicode/utf8"
)

type v1sec struct {
	kind byte
	off  uint64
	body []byte
}

type v1meta struct {
	sha                     [32]byte
	size, duration, frames  uint64
	demoinfo                []byte
	haveSha, haveSize, tmng bool
}

type v1seg struct {
	sec                    v1sec
	rawOff, rawLen, tStart uint64
	crc                    uint32
	coding                 byte
	bodyOff                int
	share                  bool
	dOff, dLen             uint64
}

func v1checkUTF8(b []byte) {
	if !utf8.Valid(b) {
		fail("utf8")
	}
}

func v1meta_(body []byte) *v1meta {
	r := &rd{b: body}
	ver := r.u8()
	codec := r.u8()
	n := r.uv()
	crc := r.u32()
	if ver != 1 || codec > 1 {
		fail("meta version/codec")
	}
	if n > 1<<24 || n > uint64(len(body))*1024+4096 {
		fail("meta size")
	}
	var p []byte
	if codec == 0 {
		p = r.b[r.p:]
		if uint64(len(p)) != n {
			fail("meta len")
		}
	} else {
		p = unzstdExact(r.b[r.p:], n)
	}
	if crc32c(p) != crc {
		fail("meta crc")
	}
	m := &v1meta{}
	seen := map[uint64]bool{}
	fr := &rd{b: p}
	for fr.left() > 0 {
		tag := fr.uv()
		v := fr.take(fr.uv())
		if tag >= 1 && tag <= 16 {
			if seen[tag] {
				fail("dup tag")
			}
			seen[tag] = true
		}
		f := &rd{b: v}
		switch tag {
		case 1:
			if len(v) != 32 {
				fail("sha len")
			}
			copy(m.sha[:], v)
			m.haveSha = true
		case 2:
			m.size = f.uv()
			m.haveSize = true
		case 3:
			if len(v) == 0 {
				fail("empty demoinfo")
			}
			for f.left() > 0 {
				f.take(uint64(f.u32()))
			}
			m.demoinfo = v
		case 8:
			m.duration = f.uv()
			m.frames = f.uv()
			m.tmng = true
		case 9:
			v1checkUTF8(v)
		case 11:
			f.uv()
			f.uv()
		case 12, 13, 15:
			f.uv()
			if f.left() > 128 {
				fail("clock string")
			}
			f.take(uint64(f.left()))
		case 16:
			tr := f.u8()
			start := f.uv()
			c := f.uv()
			if tr > 1 || c > 65536 || c > uint64(len(v)/5) || start > c || (c == 0 && tr == 0) {
				fail("player log")
			}
			var last, bytes uint64
			for i := uint64(0); i < c; i++ {
				t := f.uv()
				if i > 0 && t < last {
					fail("player log order")
				}
				last = t
				if f.uv() > 255 {
					fail("player log")
				}
				switch f.u8() {
				case 0:
					if f.uv() > 0xFFFFFFFF {
						fail("player log")
					}
					n := f.uv()
					f.take(n)
					bytes += n
				case 1:
					n := f.uv()
					f.take(n)
					k := f.uv()
					f.take(k)
					bytes += n + k
				default:
					fail("player log")
				}
				if bytes > 1<<20 {
					fail("player log")
				}
			}
		case 14:
			tr := f.u8()
			c := f.uv()
			if tr > 1 || c == 0 || c > 65536 {
				fail("pauses")
			}
			var last uint64
			for i := uint64(0); i < c; i++ {
				t := f.uv()
				if i > 0 && t <= last {
					fail("pause order")
				}
				last = t
				f.uv()
			}
		case 10:
			c := f.uv()
			keys := map[string]bool{}
			for i := uint64(0); i < c; i++ {
				k := f.take(f.uv())
				v1checkUTF8(k)
				if keys[string(k)] {
					fail("dup ext key")
				}
				keys[string(k)] = true
				f.take(f.uv())
			}
		default:
			continue
		}
		if (tag == 2 || tag == 16 || tag == 8 || tag == 10 || tag == 11 || tag == 14) && f.left() != 0 {
			fail("tag value not consumed")
		}
	}
	if !m.haveSha || !m.haveSize || !m.tmng {
		fail("identity tags")
	}
	return m
}

func decodeV1(file []byte) (out []byte, err error) {
	defer catch(&err)
	if len(file) < 5 || string(file[:4]) != "MVDZ" || file[4] != 1 {
		fail("not an MVDZ v1 file")
	}
	if len(file) < 16 {
		fail("truncated prefix")
	}
	pr := &rd{b: file, p: 8}
	prefMeta := pr.u64()
	var meta *v1meta
	var metaOff, indexOff uint64
	var haveMeta, haveIndex, haveTables bool
	// TABLES (§2.6): the file's shared rANS models by stream id
	tables := map[uint64]*ransModel{}
	var ix struct {
		size, dur uint64
		crc       uint32
		segs      [][3]uint64
	}
	var segs []v1seg
	var raw, dnext uint64
	var trailer *rd
	off := uint64(16)
	for off < uint64(len(file)) {
		if trailer != nil {
			fail("section after trailer")
		}
		if uint64(len(file))-off < 9 {
			break
		}
		r := &rd{b: file, p: int(off)}
		kind := r.u8()
		l := r.u64()
		if l > uint64(len(file))-off-9 {
			break
		}
		body := file[off+9 : off+9+l]
		switch kind {
		case 0x01:
			if haveMeta {
				fail("meta order")
			}
			if l > 1<<24+64 {
				fail("meta len")
			}
			meta = v1meta_(body)
			haveMeta, metaOff = true, off
		case 0x03:
			if haveIndex {
				fail("index order")
			}
			if l > 1<<24 {
				fail("index len")
			}
			x := &rd{b: body}
			ix.size = x.uv()
			ix.crc = x.u32()
			ix.dur = x.uv()
			n := x.uv()
			if n > uint64(len(body)/3) {
				fail("index count")
			}
			var tot uint64
			for i := uint64(0); i < n; i++ {
				e := [3]uint64{x.uv(), x.uv(), x.uv()}
				tot += e[1]
				ix.segs = append(ix.segs, e)
			}
			if x.left() != 0 || tot != ix.size {
				fail("index")
			}
			haveIndex, indexOff = true, off
		case 0x82:
			if haveIndex {
				fail("segment order")
			}
			x := &rd{b: body}
			s := v1seg{sec: v1sec{kind, off, body}}
			s.rawOff, s.rawLen, s.tStart = x.uv(), x.uv(), x.uv()
			s.crc = x.u32()
			s.coding = x.u8()
			s.bodyOff = x.p
			switch s.coding {
			case 0:
				fl := x.uv()
				s.dOff, s.dLen = x.uv(), x.uv()
				s.share = fl&1 != 0
				if fl&^1 != 0 || (!s.share && (s.dOff != 0 || s.dLen != 0)) || (s.share && s.dLen == 0) {
					fail("segment flags/share")
				}
			case 1, 2:
			default:
				fail("coding")
			}
			if s.rawOff != raw || s.rawLen > 0xFFFF0000 || s.rawLen > l*1024+4096 {
				fail("segment header")
			}
			if s.share {
				if !haveMeta || s.dOff != dnext || s.dOff+s.dLen > uint64(len(meta.demoinfo)) {
					fail("share")
				}
				dnext = s.dOff + s.dLen
			}
			raw += s.rawLen
			segs = append(segs, s)
		case 0x86:
			if haveTables || haveIndex || len(segs) > 0 {
				fail("tables order")
			}
			if l > 1<<24 {
				fail("tables len")
			}
			x := &rd{b: body}
			n := x.uv()
			if n == 0 {
				fail("tables count")
			}
			prev := int64(-1)
			for i := uint64(0); i < n; i++ {
				sid := x.uv()
				if int64(sid) <= prev || !(isLazy(sid) || isNum(sid)) {
					fail("tables id")
				}
				alpha := uint32(256)
				if isNum(sid) {
					alpha = 65
				}
				tables[sid] = readModelV1(x.take(x.uv()), alpha, 65535)
				prev = int64(sid)
			}
			if x.left() != 0 {
				fail("tables trailing")
			}
			haveTables = true
		case 0x7F:
			if l != 24 || off+33 != uint64(len(file)) || string(body[16:]) != "MVDZTRL1" {
				fail("trailer")
			}
			trailer = &rd{b: body}
		default:
			if kind&0x80 != 0 {
				fail("unknown critical section")
			}
		}
		off += 9 + l
	}
	if trailer == nil || !haveMeta || !haveIndex {
		fail("incomplete")
	}
	tIdx, tMeta := trailer.u64(), trailer.u64()
	if tMeta != metaOff || (prefMeta != 0 && prefMeta != metaOff) || tIdx != indexOff {
		fail("trailer offsets")
	}
	if len(ix.segs) != len(segs) || ix.size != raw || meta.size != ix.size || dnext != uint64(len(meta.demoinfo)) {
		fail("index/segments")
	}
	for i, e := range ix.segs {
		if e[0] != segs[i].sec.off || e[1] != segs[i].rawLen || e[2] != segs[i].tStart {
			fail("index entry")
		}
	}
	// content
	out = make([]byte, 0, ix.size)
	var t, frames uint64
	open := false
	for _, s := range segs {
		if open {
			fail("segment ends inside a frame")
		}
		dst := make([]byte, s.rawLen)
		coded := s.sec.body[s.bodyOff:]
		switch s.coding {
		case 0:
			var share []byte
			if s.share {
				share = meta.demoinfo[s.dOff : s.dOff+s.dLen]
			}
			decodeV1Segment(coded, dst, share, tables)
		case 1:
			copy(dst, unzstdExact(coded, s.rawLen))
		case 2:
			if uint64(len(coded)) != s.rawLen {
				fail("store len")
			}
			copy(dst, coded)
		}
		if crc32c(dst) != s.crc {
			fail("segment crc")
		}
		if s.tStart != t {
			fail("t_start")
		}
		p := 0
		for {
			n := frameLen(dst, p)
			if n == 0 {
				break
			}
			t += uint64(dst[p])
			frames++
			p += n
		}
		open = p != len(dst)
		out = append(out, dst...)
	}
	if crc32c(out) != ix.crc || t != ix.dur || t != meta.duration || frames != meta.frames {
		fail("whole-demo checks")
	}
	if sha256.Sum256(out) != meta.sha || uint64(len(out)) != meta.size {
		fail("identity")
	}
	return out, nil
}

// frameLen: FORMAT.md §6.2 frame_len; 0 = not a frame.
func frameLen(b []byte, p int) int {
	rem := len(b) - p
	if rem < 2 {
		return 0
	}
	switch b[p+1] & 7 {
	case 2:
		if rem >= 10 {
			return 10
		}
	case 3:
		if rem >= 10 {
			n := uint64(le32(b[p+6:]))
			if n <= uint64(rem-10) {
				return 10 + int(n)
			}
		}
	case 1, 4, 5, 6:
		if rem >= 6 {
			n := uint64(le32(b[p+2:]))
			if n <= uint64(rem-6) {
				return 6 + int(n)
			}
		}
	}
	return 0
}

// rANS stream: the coder of coder.go with the end-of-stream checks of
// FORMAT.md §8.3.
type ransV1 struct{ ransCore }

func (r *ransV1) done() bool {
	if r.n != r.count || r.wp != len(r.words) {
		return false
	}
	for _, x := range r.X {
		if x != 1<<16 {
			return false
		}
	}
	return (r.extra.k+7)/8 == uint64(len(r.extra.b))
}

// newRansV1: back end 2 (own model header) or, with a file model m, back
// end 3 (the body without the header).
func newRansV1(body []byte, count uint64, numeric bool, m *ransModel) *ransV1 {
	rr := &rd{b: body}
	if m == nil {
		hdr := rr.take(rr.uv())
		alpha := uint32(256)
		if numeric {
			alpha = 65
		}
		m = readModelV1(hdr, alpha, count)
	}
	words := rr.take(rr.uv())
	r := &ransV1{ransCore{m: m, words: words, count: count}}
	r.extra = &bitRd{b: rr.b[rr.p:]}
	if !numeric && len(r.extra.b) != 0 {
		fail("extra on a structural stream")
	}
	for j := 0; j < 4; j++ {
		hi := r.word()
		lo := r.word()
		r.X[j] = hi<<16 | lo
	}
	return r
}

type lebNumV1 struct {
	lebNum
	left uint64
}

func (s *lebNumV1) val(f [6]uint8, sid int) int32 {
	if s.left == 0 {
		fail("numeric store exhausted")
	}
	s.left--
	if s.p < len(s.b) {
		// a 5th byte above 0x0F is an error (FORMAT.md §8.1)
		q := s.p
		for k := 0; k < 4 && q < len(s.b) && s.b[q] >= 0x80; k++ {
			q++
		}
		if q-s.p == 4 && q < len(s.b) && s.b[q] > 0x0F {
			fail("leb")
		}
	}
	return s.lebNum.val(f, sid)
}
func (s *lebNumV1) done() bool { return s.left == 0 && s.p == len(s.b) }

func decodeV1Segment(body []byte, dst []byte, share []byte, tables map[uint64]*ransModel) {
	r := &rd{b: body}
	flags := r.uv()
	dOff, dLen := r.uv(), r.uv()
	fte, fte2, mvd1 := r.uv(), r.uv(), r.uv()
	tail := r.uv()
	n := r.uv()
	if flags&^1 != 0 || (flags&1 == 0 && (dOff != 0 || dLen != 0)) || fte > 0xFFFFFFFF || fte2 > 0xFFFFFFFF || mvd1 > 0xFFFFFFFF {
		fail("segment header")
	}
	if tail > uint64(len(dst)) || n > 384 {
		fail("tail/n")
	}
	ents := make([]streamEntry, n)
	seen := map[uint64]bool{}
	var total uint64
	for i := range ents {
		e := &ents[i]
		e.id, e.backend, e.count, e.plen = r.uv(), r.uv(), r.uv(), r.uv()
		if !(isLazy(e.id) || isNum(e.id) || isBlob(e.id)) || seen[e.id] || e.count > uint64(len(dst))+16 {
			fail("stream entry")
		}
		if isBlob(e.id) && e.backend > 1 || e.backend > 3 {
			fail("backend")
		}
		seen[e.id] = true
		total += e.plen
	}
	if total != uint64(r.left()) {
		fail("bodies")
	}
	for i := range ents {
		ents[i].body = r.take(ents[i].plen)
	}
	blobs := map[int]*blobCur{}
	lazies := map[int]lazyDec{}
	nums := map[int]numDec{}
	for _, e := range ents {
		switch {
		case isBlob(e.id):
			if e.backend == 0 {
				if uint64(len(e.body)) != e.count {
					fail("store blob")
				}
				blobs[int(e.id)] = &blobCur{b: e.body}
			} else {
				blobs[int(e.id)] = &blobCur{b: unzstdExact(e.body, e.count)}
			}
		case isLazy(e.id):
			switch e.backend {
			case 0:
				if uint64(len(e.body)) != e.count {
					fail("store lazy")
				}
				lazies[int(e.id)] = &bytesLazy{b: e.body}
			case 1:
				lazies[int(e.id)] = &bytesLazy{b: unzstdExact(e.body, e.count)}
			case 2:
				lazies[int(e.id)] = newRansV1(e.body, e.count, false, nil)
			case 3:
				lazies[int(e.id)] = newRansV1(e.body, e.count, false, fileModel(tables, e.id))
			}
		case isNum(e.id):
			switch e.backend {
			case 0:
				nums[int(e.id)] = &lebNumV1{lebNum{b: e.body}, e.count}
			case 1:
				b := unzstd(e.body, 5*e.count)
				if uint64(len(b)) > 5*e.count {
					fail("numeric zstd size")
				}
				nums[int(e.id)] = &lebNumV1{lebNum{b: b}, e.count}
			case 2:
				nums[int(e.id)] = newRansV1(e.body, e.count, true, nil)
			case 3:
				nums[int(e.id)] = newRansV1(e.body, e.count, true, fileModel(tables, e.id))
			}
		}
	}
	sym := func(L int, ctx uint32) byte {
		d := lazies[L]
		if d == nil {
			fail("absent structural stream %d", L)
		}
		return d.sym(ctx)
	}
	w := newWalker(dst, sym, blobs, newProto(uint32(fte), uint32(fte2), uint32(mvd1)))
	if flags&1 != 0 {
		w.share = true
		w.dinfo = &rd{b: share}
	}
	w.run(int(tail))
	if w.dinfo != nil && w.dinfo.left() != 0 {
		fail("share not consumed")
	}
	for _, d := range lazies {
		if !d.done() {
			fail("structural stream not complete")
		}
	}
	for _, b := range blobs {
		if b.p != len(b.b) {
			fail("blob not consumed")
		}
	}
	val := func(L int, f [6]uint8, lane int) int32 {
		d := nums[L]
		if d == nil {
			fail("absent numeric stream %d", L)
		}
		return d.val(f, L)
	}
	numericV2(dst, w, val)
	for _, d := range nums {
		if !d.done() {
			fail("numeric stream not complete")
		}
	}
}

var _ = bytes.Equal

// fileModel: the TABLES model of a back end 3 stream (§8.4).
func fileModel(tables map[uint64]*ransModel, id uint64) *ransModel {
	m := tables[id]
	if m == nil {
		fail("back end 3 without a TABLES entry")
	}
	return m
}
