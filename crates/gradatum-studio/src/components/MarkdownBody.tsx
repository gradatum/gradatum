/**
 * MarkdownBody — rendu markdown sanitizé via rehype-sanitize
 *
 * XSS #1 : rehype-sanitize est OBLIGATOIRE sur tout contenu markdown rendu
 * Source : s02-verdict-fige.md §5 + plan §risques R1
 */

import { useEffect, useState } from 'react';
import { unified } from 'unified';
import remarkParse from 'remark-parse';
import remarkRehype from 'remark-rehype';
import rehypeSanitize from 'rehype-sanitize';
import rehypeStringify from 'rehype-stringify';

interface MarkdownBodyProps {
  content: string;
  style?: React.CSSProperties;
  className?: string;
}

export function MarkdownBody({ content, style, className }: MarkdownBodyProps) {
  const [html, setHtml] = useState('');

  useEffect(() => {
    let cancelled = false;
    unified()
      .use(remarkParse)
      .use(remarkRehype)
      .use(rehypeSanitize) // XSS protection obligatoire
      .use(rehypeStringify)
      .process(content)
      .then(result => {
        if (!cancelled) {
          setHtml(String(result));
        }
      })
      .catch(_err => {
        // Fallback sécurisé : afficher le texte brut si le pipeline échoue
        if (!cancelled) setHtml('');
      });
    return () => { cancelled = true; };
  }, [content]);

  return (
    <div
      className={className}
      style={{
        fontSize: '14.5px',
        lineHeight: 1.7,
        color: '#33312c',
        ...style,
      }}
      // biome-ignore lint/security/noDangerouslySetInnerHtml: sanitizé par rehype-sanitize
      dangerouslySetInnerHTML={{ __html: html }}
      data-testid="markdown-body"
    />
  );
}
