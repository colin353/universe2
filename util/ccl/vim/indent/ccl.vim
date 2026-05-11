setlocal indentexpr=GetCCLIndent()
setlocal indentkeys=0{,0},0),0[,0],!^F,o,O,e

" Check if line 'lnum' has more opening brackets than closing ones.
function s:LineHasOpeningBrackets(lnum)
  let open_0 = 0
  let open_2 = 0
  let open_4 = 0
  let line = getline(a:lnum)
  let pos = match(line, '[][(){}]', 0)
  while pos != -1
    let idx = stridx('(){}[]', line[pos])
    if idx % 2 == 0
      let open_{idx} = open_{idx} + 1
    else
      let open_{idx - 1} = open_{idx - 1} - 1
    endif
    let pos = match(line, '[][(){}]', pos + 1)
  endwhile
  return (open_0 > 0) . (open_2 > 0) . (open_4 > 0)
endfunction

function GetCCLIndent()
  let vcol = col('.')
  let line = getline(v:lnum)
  let ind = -1
  let col = matchend(line, '^\s*[]}]')

  if col > 0
    call cursor(v:lnum, col)
    let bs = strpart('{}[]', stridx('}]', line[col - 1]) * 2, 2)

    let pairstart = escape(bs[0], '[')
    let pairend = escape(bs[1], ']')
    let pairline = searchpair(pairstart, '', pairend, 'bW')

    if pairline > 0
      let ind = indent(pairline)
    else
      let ind = virtcol('.') - 1
    endif

    return ind
  endif

  let lnum = prevnonblank(v:lnum - 1)

  if lnum == 0
    return 0
  endif

  let line = getline(lnum)
  let ind = indent(lnum)

  if line =~ '[[({]'
    let counts = s:LineHasOpeningBrackets(lnum)
    if counts[0] == '1' || counts[1] == '1' || counts[2] == '1'
      if exists('*shiftwidth')
        return ind + shiftwidth()
      else
        return ind + &sw
      endif
    else
      call cursor(v:lnum, vcol)
    end
  endif

  return ind
endfunction

