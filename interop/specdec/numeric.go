package main

import (
	"encoding/binary"
	"math"
)

// Numeric groups: FORMAT.md §7.

// Numeric stream ids (FORMAT.md §5.4).
const (
	nPIO  = 48 // +a
	nPIA  = 51
	nPEO  = 54
	nSNDO = 57
	nTEO  = 58
	nSTL  = 59
	nPIF  = 60
	nPEF  = 63
	nUCA  = 66
)

type valFn func(L int, f [6]uint8, lane int) int32

type Track struct {
	last, prev, prev2 uint16
	res               int32
	flast, fprev      uint32
	fres              int32
	tLast             uint32
}

func rd16(out []byte, off uint32) uint16 {
	if uint64(off)+2 > uint64(len(out)) {
		return 0
	}
	return uint16(out[off]) | uint16(out[off+1])<<8
}

func x16(out []byte, off uint32, pred uint16, r int32) (uint16, int32) {
	v := pred + uint16(r)
	if uint64(off)+2 <= uint64(len(out)) {
		out[off] = byte(v)
		out[off+1] = byte(v >> 8)
	}
	return v, int32(int16(uint16(r)))
}

func x32(out []byte, off uint32, pred uint32, r int32) (uint32, int32) {
	v := pred + uint32(r)
	if uint64(off)+4 <= uint64(len(out)) {
		out[off] = byte(v)
		out[off+1] = byte(v >> 8)
		out[off+2] = byte(v >> 16)
		out[off+3] = byte(v >> 24)
	}
	return v, r
}

func dd(now, prev int32) uint8 { return uint8(clampI(now-prev, -3, 3) + 3) }

func divRound(a, b int32) int32 {
	abs := uint32(a)
	if a < 0 {
		abs = uint32(-a)
	}
	n := abs + uint32(b)/2
	qq := int32(n / uint32(b))
	if a < 0 {
		return -qq
	}
	return qq
}

func predDt(last, prev uint16, dtNow, dtPrev int32) uint16 {
	d := int32(int16(last - prev))
	if dtNow == dtPrev || dtPrev <= 0 || dtNow < 0 {
		return last + uint16(d)
	}
	return last + uint16(divRound(d*min(dtNow, 32767), min(dtPrev, 32767)))
}

type tm struct{ last, prev uint32 }

func times(m *tm, t uint32) (int32, int32) {
	now := int32(t - m.last)
	pv := int32(m.last - m.prev)
	m.prev, m.last = m.last, t
	return now, pv
}

func clamp63(v int32) uint8 { return uint8(clampI(v, 0, 63)) }

// FORMAT.md §7.3
func piOriginStep(out []byte, r PiRec, a int, s *Track, dtNow, dtPrev int32, val valFn, lane int) {
	sent := r.flags&(1<<a) != 0
	if r.fl&1 != 0 {
		if sent {
			off := r.off + 4*uint32(popc(uint32(r.flags)&((1<<a)-1)))
			pred := s.flast + (s.flast - s.fprev)
			f := [6]uint8{q(s.fres), uint8(a)}
			v, rr := x32(out, off, pred, val(nPIF+a, f, lane))
			s.fprev = s.flast
			s.flast = v
			s.fres = rr
		} else {
			s.fprev = s.flast
			s.fres = 0
		}
		return
	}
	var v uint16
	if sent {
		pred := predDt(s.last, s.prev, dtNow, dtPrev)
		off := r.off + 2*uint32(popc(uint32(r.flags)&((1<<a)-1)))
		vel := int32(int16(s.last - s.prev))
		f := [6]uint8{q(s.res), q(vel), clamp63(dtNow), uint8(r.flags & 7), dd(dtNow, dtPrev), 0}
		v, s.res = x16(out, off, pred, val(nPIO+a, f, lane))
	} else {
		s.res = 0
		v = s.last
	}
	s.prev = s.last
	s.last = v
}

// FORMAT.md §7.4
func piAngleStep(out []byte, r PiRec, a int, s *Track, dtNow, dtPrev int32, val valFn, lane int) {
	var v uint16
	if r.flags&(8<<a) != 0 {
		pred := predDt(s.last, s.prev, dtNow, dtPrev)
		cw := uint32(2)
		if r.fl&1 != 0 {
			cw = 4
		}
		off := r.off + cw*uint32(popc(uint32(r.flags)&7)) + 2*uint32(popc((uint32(r.flags)>>3)&((1<<a)-1)))
		dprev := int32(int16(s.prev - s.prev2))
		f := [6]uint8{q(s.res), q(dprev), clamp63(dtNow), uint8((r.flags >> 3) & 7), dd(dtNow, dtPrev), 0}
		v, s.res = x16(out, off, pred, val(nPIA+a, f, lane))
	} else {
		s.res = 0
		v = s.last
	}
	s.prev2 = s.prev
	s.prev = s.last
	s.last = v
}

func originOffs(r PeRec) [3]uint32 {
	cw := uint32(2)
	if r.bits&128 != 0 {
		cw = 4
	}
	aw := uint32(1)
	if r.bits&256 != 0 {
		aw = 2
	}
	pos := r.off
	var o [3]uint32
	for i := 0; i < 3; i++ {
		if r.bits&(1<<(2*i)) != 0 {
			o[i] = pos
			pos += cw
		}
		if r.bits&(2<<(2*i)) != 0 {
			pos += aw
		}
	}
	return o
}

// FORMAT.md §7.5
func peOriginStep(out []byte, r PeRec, a int, s *Track, val valFn, lane int) {
	dtNow := int32(r.t - s.tLast)
	s.tLast = r.t
	if r.bits&(1<<(2*a)) != 0 {
		off := originOffs(r)[a]
		if r.bits&128 != 0 {
			pred := s.flast + (s.flast - s.fprev)
			f := [6]uint8{q(s.fres), uint8(a)}
			v, rr := x32(out, off, pred, val(nPEF+a, f, lane))
			s.fprev = s.flast
			s.flast = v
			s.fres = rr
		} else {
			vel := int32(int16(s.last - s.prev))
			pred := s.last + uint16(vel)
			ip := uint8(0)
			if r.bits&64 != 0 {
				ip = 1
			}
			f := [6]uint8{q(s.res), q(vel), r.model, ip | (r.ubits&7)<<1, clamp63(dtNow), 0}
			var v uint16
			v, s.res = x16(out, off, pred, val(nPEO+a, f, lane))
			s.prev = s.last
			s.last = v
		}
	} else {
		s.prev = s.last
		s.res = 0
		s.fprev = s.flast
		s.fres = 0
	}
}

// FORMAT.md §7.6
func stlAll(out []byte, recs []StlRec, val valFn) {
	var last [8192]uint32
	var lres [8192]int32
	for _, r := range recs {
		k := r.key & 0x1fff
		f := [6]uint8{uint8(r.key & 0xff), q(lres[k])}
		last[k], lres[k] = x32(out, r.off, last[k], val(nSTL, f, -1))
	}
}

// FORMAT.md §7.6: usercmd angles, s * 45/8192 as f32 (exact)
func ucAll(out []byte, recs []UcRec, val valFn) {
	var tr [256][3]Track
	for _, r := range recs {
		for a := 0; a < 3; a++ {
			if r.grid&(1<<a) == 0 {
				continue
			}
			t := &tr[r.num][a]
			d := int32(int16(t.last - t.prev))
			d2 := int32(int16(t.prev - t.prev2))
			pred := t.last + uint16(d)
			f := [6]uint8{q(t.res), q(d), q(d2), uint8(a)}
			v := pred + uint16(val(nUCA, f, -1))
			bits := math.Float32bits(float32(int16(v)) * (45.0 / 8192.0))
			off := r.off + 4*uint32(a)
			if uint64(off)+4 <= uint64(len(out)) {
				binary.LittleEndian.PutUint32(out[off:], bits)
			}
			t.res = int32(int16(v - pred))
			t.prev2, t.prev, t.last = t.prev, t.last, v
		}
	}
}

func sndAll(out []byte, recs []SndRec, val valFn) {
	for _, r := range recs {
		for i := 0; i < 3; i++ {
			pred := uint16(0)
			if r.refs[i] != NONE {
				pred = rd16(out, r.refs[i])
			}
			f := [6]uint8{uint8(i) | r.kind<<2, r.num}
			x16(out, r.off+2*uint32(i), pred, val(nSNDO, f, -1))
		}
	}
}

func teAll(out []byte, recs []TeRec, val valFn) {
	var last [3]uint16
	for _, r := range recs {
		if isBeam(r.ty) {
			var start [3]uint16
			for i := 0; i < 3; i++ {
				pred := uint16(0)
				if r.refs[i] != NONE {
					pred = rd16(out, r.refs[i])
				}
				f := [6]uint8{uint8(i) | 1<<2, r.ty}
				start[i], _ = x16(out, r.off+2*uint32(i), pred, val(nTEO, f, -1))
			}
			for i := 0; i < 3; i++ {
				f := [6]uint8{uint8(i) | 2<<2, r.ty}
				last[i], _ = x16(out, r.off+6+2*uint32(i), start[i], val(nTEO, f, -1))
			}
		} else {
			for i := 0; i < 3; i++ {
				f := [6]uint8{uint8(i), r.ty}
				last[i], _ = x16(out, r.off+2*uint32(i), last[i], val(nTEO, f, -1))
			}
		}
	}
}

// num_ctx: FORMAT.md §8.3, "Contexts and symbols"
func numCtx(L int, f [6]uint8) uint32 {
	cl := func(v uint8, m uint8) uint32 { return uint32(min(v, m)) }
	switch {
	case L >= 48 && L <= 50:
		return cl(f[0], 19) | cl(f[1], 19)<<5
	case L >= 51 && L <= 53:
		return cl(f[0], 23) | cl(f[1], 23)<<5
	case L >= 54 && L <= 56:
		return cl(f[0], 17) | cl(f[1], 17)<<5 | uint32(f[3]&1)<<10
	case L == 57 || L == 58:
		return uint32(f[0]) | uint32(f[1])<<4
	case L == 59:
		return uint32(f[0]) | cl(f[1], 15)<<8
	case L == 66:
		return cl(f[0], 23) | cl(f[1], 23)<<5 | cl(f[2], 15)<<10 | uint32(f[3])<<14
	}
	return cl(f[0], 63) | uint32(f[1])<<6
}

// Wave order (FORMAT.md §7.7, §12 item 1): everything in record order.
func numericV2(out []byte, w *Walker, val valFn) {
	for a := 0; a < 3; a++ {
		var tms [256]tm
		var tr [256]Track
		for _, r := range w.pis {
			now, pv := times(&tms[r.num], r.t)
			piOriginStep(out, r, a, &tr[r.num], now, pv, val, -1)
		}
	}
	for a := 0; a < 3; a++ {
		var tms [256]tm
		var tr [256]Track
		for _, r := range w.pis {
			now, pv := times(&tms[r.num], r.t)
			piAngleStep(out, r, a, &tr[r.num], now, pv, val, -1)
		}
	}
	for a := 0; a < 3; a++ {
		tr := make([]Track, 2048)
		for _, r := range w.pes {
			peOriginStep(out, r, a, &tr[r.num&2047], val, -1)
		}
	}
	stlAll(out, w.stls, val)
	ucAll(out, w.ucs, val)
	sndAll(out, w.snds, val)
	teAll(out, w.tes, val)
}
